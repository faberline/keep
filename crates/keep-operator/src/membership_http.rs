//! The membership admin API of keep over HTTP, called by the operator's
//! `reconcile_plan` before the StatefulSet changes its pods.
//!
//! The calls go to the pod DNS name of each shard candidate. A refusal body is
//! `{"code": "...", "message": "..."}` (`ApiError` in
//! `crates/keep-commands/src/http/error.rs`). Codes that can come back:
//! `not_leader` (409), `unknown_node` (404), `unroutable_peer` (400),
//! `not_caught_up`, `last_voter`, `would_lose_tolerance`, `is_leader`,
//! `transition_in_flight` (409), `shard_not_owned` (409), `raft_disabled` (404).

use std::time::Duration;

use reqwest::{Client, Method};
use serde_json::{json, Value};

use crate::scale::{MembershipClient, Refusal};

/// Port of the keep HTTP server in a pod, as `render.rs` `CLIENT_PORT`.
const HTTP_PORT: u16 = 7117;

/// The membership admin API over HTTP, one candidate list per shard.
pub struct HttpMembershipClient {
    http: Client,
    /// `endpoints[shard]` = base urls (`http://host:port`, no trailing slash)
    /// of the pods that can lead that shard.
    endpoints: Vec<Vec<String>>,
}

impl HttpMembershipClient {
    /// Candidates for every shard: replicas `0..members_per_shard`, base url
    /// `http://<sts>-<ordinal>.<sts>-headless:7117` with ordinal
    /// `replica * shard_count + shard` (the same pod math as `scale::peer_url`).
    pub fn new(sts: &str, shard_count: u32, members_per_shard: u32) -> Self {
        Self {
            http: build_client(),
            endpoints: endpoint_list(sts, shard_count, members_per_shard),
        }
    }

    /// Explicit candidate lists: `endpoints[shard]`.
    pub fn with_endpoints(endpoints: Vec<Vec<String>>) -> Self {
        Self {
            http: build_client(),
            endpoints,
        }
    }

    /// Send one membership call to the candidates of `shard`, in order. A
    /// `not_leader` refusal or an unreachable pod moves on to the next candidate;
    /// any other refusal is returned at once.
    async fn call(
        &self,
        shard: u32,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> Result<(), Refusal> {
        let bases = self
            .endpoints
            .get(shard as usize)
            .filter(|bases| !bases.is_empty())
            .ok_or_else(|| Refusal {
                status: 0,
                code: "no_endpoint".into(),
            })?;
        let mut last = Refusal {
            status: 0,
            code: "unreachable".into(),
        };
        for base in bases {
            let mut request = self.http.request(method.clone(), format!("{base}{path}"));
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) => {
                    last = Refusal {
                        status: 0,
                        code: "unreachable".into(),
                    };
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            let text = response.text().await.unwrap_or_default();
            let code = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value["code"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("http_{}", status.as_u16()));
            let refusal = Refusal {
                status: status.as_u16(),
                code,
            };
            if refusal.code == "not_leader" {
                last = refusal;
                continue;
            }
            return Err(refusal);
        }
        Err(last)
    }
}

fn build_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client builds")
}

fn endpoint_list(sts: &str, shard_count: u32, members_per_shard: u32) -> Vec<Vec<String>> {
    (0..shard_count)
        .map(|shard| {
            (0..members_per_shard)
                .map(|replica| {
                    let ordinal = replica * shard_count + shard;
                    format!("http://{sts}-{ordinal}.{sts}-headless:{HTTP_PORT}")
                })
                .collect()
        })
        .collect()
}

impl MembershipClient for HttpMembershipClient {
    async fn add_learner(&self, shard: u32, node_id: u64, url: &str) -> Result<(), Refusal> {
        self.call(
            shard,
            Method::POST,
            format!("/admin/raft/{shard}/members"),
            Some(json!({ "node_id": node_id, "url": url })),
        )
        .await
    }

    async fn promote(&self, shard: u32, node_id: u64) -> Result<(), Refusal> {
        self.call(
            shard,
            Method::POST,
            format!("/admin/raft/{shard}/members/{node_id}/promote"),
            None,
        )
        .await
    }

    async fn remove(&self, shard: u32, node_id: u64) -> Result<(), Refusal> {
        self.call(
            shard,
            Method::DELETE,
            format!("/admin/raft/{shard}/members/{node_id}"),
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// A local stub: answers each connection with the next scripted
    /// `(status, body)`. Returns its base url and the request lines it saw.
    async fn stub(responses: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let request = read_request(&mut socket).await;
                let line = request.lines().next().unwrap_or_default().to_string();
                recorded.lock().expect("seen lock").push(line);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.expect("write");
                socket.shutdown().await.ok();
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// Read one HTTP request: the head, then the body by `content-length`.
    async fn read_request(socket: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = socket.read(&mut chunk).await.expect("read");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let length = head
                    .lines()
                    .find_map(|l| {
                        let lower = l.to_ascii_lowercase();
                        lower
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    #[tokio::test]
    async fn add_learner_posts_the_body_and_succeeds() {
        let (base, seen) = stub(vec![(200, "{\"shard\":0,\"node_id\":1,\"index\":7}")]).await;
        let client = HttpMembershipClient::with_endpoints(vec![vec![base]]);
        let result = client
            .add_learner(0, 1, "http://k-1.k-headless:7117/shard/0")
            .await;
        assert_eq!(result, Ok(()));
        let seen = seen.lock().expect("seen lock");
        assert!(seen[0].starts_with("POST /admin/raft/0/members "), "{}", seen[0]);
    }

    #[tokio::test]
    async fn a_409_not_caught_up_maps_to_a_refusal() {
        let (base, seen) = stub(vec![(
            409,
            "{\"code\":\"not_caught_up\",\"message\":\"x\"}",
        )])
        .await;
        let client = HttpMembershipClient::with_endpoints(vec![vec![base]]);
        assert_eq!(
            client.promote(0, 1).await,
            Err(Refusal {
                status: 409,
                code: "not_caught_up".into()
            })
        );
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1, "a refusal other than not_leader is not retried");
        assert!(
            seen[0].starts_with("POST /admin/raft/0/members/1/promote "),
            "{}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn not_leader_is_retried_once_on_the_next_replica() {
        let (first, seen_first) = stub(vec![(
            409,
            "{\"code\":\"not_leader\",\"message\":\"x\"}",
        )])
        .await;
        let (second, seen_second) = stub(vec![(200, "{}")]).await;
        let client = HttpMembershipClient::with_endpoints(vec![vec![first, second]]);
        assert_eq!(client.remove(0, 2).await, Ok(()));
        let expected = "DELETE /admin/raft/0/members/2 HTTP/1.1";
        assert_eq!(seen_first.lock().expect("lock")[0], expected);
        assert_eq!(seen_second.lock().expect("lock")[0], expected);
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_skipped() {
        let (base, _seen) = stub(vec![(200, "{}")]).await;
        let client =
            HttpMembershipClient::with_endpoints(vec![vec!["http://127.0.0.1:1".into(), base]]);
        assert_eq!(client.remove(0, 2).await, Ok(()));
    }

    #[tokio::test]
    async fn a_missing_shard_is_a_refusal() {
        let client = HttpMembershipClient::with_endpoints(vec![]);
        assert_eq!(
            client.promote(3, 1).await,
            Err(Refusal {
                status: 0,
                code: "no_endpoint".into()
            })
        );
    }

    #[test]
    fn new_builds_the_pod_math() {
        assert_eq!(
            endpoint_list("k", 2, 2),
            vec![
                vec![
                    "http://k-0.k-headless:7117".to_string(),
                    "http://k-2.k-headless:7117".to_string(),
                ],
                vec![
                    "http://k-1.k-headless:7117".to_string(),
                    "http://k-3.k-headless:7117".to_string(),
                ],
            ]
        );
    }
}
