//! The keep membership admin API over HTTP, the real client of the scale planner
//! in [`crate::scale`].

use std::time::Duration;

use reqwest::{Client, Method};
use serde_json::{json, Value};

use crate::scale::{MembershipClient, Refusal};

/// Port of the keep HTTP server in a pod, as `render.rs` `CLIENT_PORT`.
const HTTP_PORT: u16 = 7117;

/// The membership admin API over HTTP, one candidate list per shard.
pub struct HttpMembershipClient {
    http: Client,
    /// `endpoints[shard]` = base urls (`http://host:port`, no trailing slash) of the pods that can lead that shard.
    endpoints: Vec<Vec<String>>,
}

impl HttpMembershipClient {
    /// Candidates for every shard: replicas `0..members_per_shard` of the StatefulSet `sts`
    /// in `namespace`. The namespace is in the url, so the name resolves from the operator's
    /// own namespace (the operator does not run where the `Keep` runs).
    pub fn new(sts: &str, namespace: &str, shard_count: u32, members_per_shard: u32) -> Self {
        Self::with_endpoints(endpoint_list(
            sts,
            namespace,
            shard_count,
            members_per_shard,
        ))
    }

    /// Explicit candidate lists, `endpoints[shard]`.
    pub fn with_endpoints(endpoints: Vec<Vec<String>>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client builds");
        Self { http, endpoints }
    }

    async fn call(
        &self,
        shard: u32,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> Result<(), Refusal> {
        let Some(bases) = self.endpoints.get(shard as usize).filter(|b| !b.is_empty()) else {
            return Err(Refusal {
                status: 0,
                code: "no_endpoint".into(),
            });
        };
        let mut last = Refusal {
            status: 0,
            code: "unreachable".into(),
        };
        for base in bases {
            let mut req = self.http.request(method.clone(), format!("{base}{path}"));
            if let Some(body) = &body {
                req = req.json(body);
            }
            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(_) => {
                    last = Refusal {
                        status: 0,
                        code: "unreachable".into(),
                    };
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            let text = resp.text().await.unwrap_or_default();
            let code = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["code"].as_str().map(str::to_string))
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

/// Base urls of the candidates of each shard: shard `s`, replica `r` is pod
/// `<sts>-<r * shard_count + s>`, the same pod math as `scale::peer_url`.
fn endpoint_list(
    sts: &str,
    namespace: &str,
    shard_count: u32,
    members_per_shard: u32,
) -> Vec<Vec<String>> {
    (0..shard_count)
        .map(|shard| {
            (0..members_per_shard)
                .map(|replica| {
                    let ordinal = replica * shard_count + shard;
                    format!("http://{sts}-{ordinal}.{sts}-headless.{namespace}.svc:{HTTP_PORT}")
                })
                .collect()
        })
        .collect()
}

impl MembershipClient for HttpMembershipClient {
    /// `POST /admin/raft/{shard}/members` with `{"node_id", "url"}`.
    async fn add_learner(&self, shard: u32, node_id: u64, url: &str) -> Result<(), Refusal> {
        self.call(
            shard,
            Method::POST,
            format!("/admin/raft/{shard}/members"),
            Some(json!({ "node_id": node_id, "url": url })),
        )
        .await
    }

    /// `POST /admin/raft/{shard}/members/{node_id}/promote`.
    async fn promote(&self, shard: u32, node_id: u64) -> Result<(), Refusal> {
        self.call(
            shard,
            Method::POST,
            format!("/admin/raft/{shard}/members/{node_id}/promote"),
            None,
        )
        .await
    }

    /// `DELETE /admin/raft/{shard}/members/{node_id}`.
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
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Read one request from `sock` (headers, then the body by content-length);
    /// returns its request line.
    async fn read_request(sock: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let head_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let mut chunk = [0u8; 1024];
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before the headers");
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse().ok())
            })
            .unwrap_or(0);
        while buf.len() < head_end + len {
            let mut chunk = [0u8; 1024];
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before the body");
            buf.extend_from_slice(&chunk[..n]);
        }
        head.lines().next().unwrap_or("").to_string()
    }

    /// A stub server that answers each request with the next scripted `(status, body)`
    /// and records each request line. Returns its base url and the request log.
    async fn stub(responses: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let line = read_request(&mut sock).await;
                seen.lock().unwrap().push(line);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(reply.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
            }
        });
        (base, log)
    }

    #[tokio::test]
    async fn add_learner_posts_the_body_and_succeeds() {
        let (base, log) = stub(vec![(200, r#"{"shard":0,"node_id":1,"index":7}"#)]).await;
        let client = HttpMembershipClient::with_endpoints(vec![vec![base]]);
        let got = client
            .add_learner(0, 1, "http://k-1.k-headless:7117/shard/0")
            .await;
        assert_eq!(got, Ok(()));
        assert!(log.lock().unwrap()[0].starts_with("POST /admin/raft/0/members "));
    }

    #[tokio::test]
    async fn a_409_not_caught_up_maps_to_a_refusal() {
        let (base, log) = stub(vec![(409, r#"{"code":"not_caught_up","message":"x"}"#)]).await;
        let client = HttpMembershipClient::with_endpoints(vec![vec![base]]);
        let got = client.promote(0, 1).await;
        assert_eq!(
            got,
            Err(Refusal {
                status: 409,
                code: "not_caught_up".into()
            })
        );
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "a non-not_leader code is not retried");
        assert!(log[0].starts_with("POST /admin/raft/0/members/1/promote "));
    }

    #[tokio::test]
    async fn not_leader_is_retried_once_on_the_next_replica() {
        let (first, log1) = stub(vec![(409, r#"{"code":"not_leader","message":"x"}"#)]).await;
        let (second, log2) = stub(vec![(200, "{}")]).await;
        let client = HttpMembershipClient::with_endpoints(vec![vec![first, second]]);
        assert_eq!(client.remove(0, 2).await, Ok(()));
        assert!(log1.lock().unwrap()[0].starts_with("DELETE /admin/raft/0/members/2 "));
        assert!(log2.lock().unwrap()[0].starts_with("DELETE /admin/raft/0/members/2 "));
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_skipped() {
        let (base, log) = stub(vec![(200, "{}")]).await;
        let client =
            HttpMembershipClient::with_endpoints(vec![vec!["http://127.0.0.1:1".into(), base]]);
        assert_eq!(client.remove(0, 2).await, Ok(()));
        assert!(log.lock().unwrap()[0].starts_with("DELETE /admin/raft/0/members/2 "));
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
    fn endpoint_list_follows_the_pod_math() {
        assert_eq!(
            endpoint_list("k", "apps", 2, 2),
            vec![
                vec![
                    "http://k-0.k-headless.apps.svc:7117".to_string(),
                    "http://k-2.k-headless.apps.svc:7117".to_string()
                ],
                vec![
                    "http://k-1.k-headless.apps.svc:7117".to_string(),
                    "http://k-3.k-headless.apps.svc:7117".to_string()
                ],
            ]
        );
    }
}
