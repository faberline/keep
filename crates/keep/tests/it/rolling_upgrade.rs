//! Rolling upgrade and rollback of a 3-replica keep group across two builds.
//! Runs three local `keep` processes. The previous build is
//! `target/previous/keep`, made by `scripts/build-previous-keep.sh`.
//! Needs `--features raft`.

#![cfg(feature = "raft")]

use std::future::Future;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The binary built from this checkout.
fn current_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_keep"))
}

/// The baseline binary. It must exist: the test fails, never skips.
fn previous_bin() -> PathBuf {
    let path = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/previous/keep"
    ));
    if !path.exists() {
        panic!(
            "previous keep binary missing at {}: run scripts/build-previous-keep.sh",
            path.display()
        );
    }
    path
}

/// Three free localhost ports. The listeners are dropped before the nodes start.
fn free_ports() -> [u16; 3] {
    let mut ports = [0u16; 3];
    for port in &mut ports {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind free port");
        *port = listener.local_addr().expect("local addr").port();
    }
    ports
}

/// Poll `check` until it returns `Some`, or panic after `timeout`.
async fn until<F, Fut, T>(what: &str, timeout: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check().await {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct Group {
    dir: TempDir,
    ports: [u16; 3],
    nodes: [Option<Child>; 3],
    client: reqwest::Client,
}

impl Group {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("tempdir"),
            ports: free_ports(),
            nodes: [None, None, None],
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client"),
        }
    }

    /// Start node `i` with `bin`. The same port and data dir are used every time.
    fn start_node(&mut self, i: usize, bin: &Path) {
        let peers = self
            .ports
            .iter()
            .map(|p| format!("http://127.0.0.1:{p}"))
            .collect::<Vec<_>>()
            .join(",");
        let data_dir = self.dir.path().join(format!("n{i}"));
        let child = Command::new(bin)
            .env("REPLICAS_PER_SHARD", "3")
            .env("VOTER_COUNT", "3")
            .env("SHARD_COUNT", "1")
            .env("KEEP_SHARD_COUNT", "1")
            .env("KEEP_REPLICAS_PER_SHARD", "3")
            .env("KEEP_RAFT_MEMBERS_PER_SHARD", "3")
            .env("KEEP_NODE_ID", i.to_string())
            .env("KEEP_NODE_COUNT", "3")
            .env("KEEP_PEERS", peers)
            .env("KEEP_GRACE_SECS", "1")
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                &self.ports[i].to_string(),
                "--data-dir",
                &data_dir.display().to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("start node {i} from {}: {e}", bin.display()));
        self.nodes[i] = Some(child);
    }

    /// Stop node `i` with SIGTERM and wait for it to exit.
    fn stop_node(&mut self, i: usize) {
        if let Some(mut child) = self.nodes[i].take() {
            let status = Command::new("kill")
                .arg("-TERM")
                .arg(child.id().to_string())
                .status()
                .expect("run kill");
            assert!(status.success(), "kill -TERM node {i} failed");
            child.wait().expect("wait for node");
        }
    }

    fn running(&self) -> Vec<usize> {
        (0..3).filter(|&i| self.nodes[i].is_some()).collect()
    }

    fn url(&self, i: usize, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.ports[i])
    }

    /// Within 30 s, a running node reports itself as the leader. Returns its index.
    async fn wait_leader(&self) -> usize {
        until("a leader", Duration::from_secs(30), || async {
            for i in self.running() {
                let body = self
                    .client
                    .get(self.url(i, "/shard/0/raftz"))
                    .send()
                    .await
                    .ok()?
                    .json::<serde_json::Value>()
                    .await
                    .ok()?;
                if body["is_leader"] == serde_json::Value::Bool(true) {
                    return Some(i);
                }
            }
            None
        })
        .await
    }

    /// Within 30 s, `/readyz` of node `i` returns 200.
    async fn wait_ready(&self, i: usize) {
        until(
            &format!("node {i} ready"),
            Duration::from_secs(30),
            || async {
                let status = self
                    .client
                    .get(self.url(i, "/readyz"))
                    .send()
                    .await
                    .ok()?
                    .status();
                (status.as_u16() == 200).then_some(())
            },
        )
        .await;
    }

    /// Within 20 s, some running node accepts the write with status 200.
    async fn put(&self, key: &str, n: u32) {
        let what = format!("write of {key}");
        until(&what, Duration::from_secs(20), || async {
            for i in self.running() {
                let resp = self
                    .client
                    .put(self.url(i, &format!("/kv/{key}")))
                    .json(&serde_json::json!({ "value": { "n": n } }))
                    .send()
                    .await;
                if let Ok(resp) = resp {
                    if resp.status().as_u16() == 200 {
                        return Some(());
                    }
                }
            }
            None
        })
        .await;
    }

    /// Within 20 s for each running node and each key, the stored number is `n`.
    async fn assert_all_keys(&self, written: &[(String, u32)]) {
        for i in self.running() {
            for (key, n) in written {
                let what = format!("{key} = {n} on node {i}");
                until(&what, Duration::from_secs(20), || async {
                    let body = self
                        .client
                        .get(self.url(i, &format!("/kv/{key}")))
                        .send()
                        .await
                        .ok()?
                        .json::<serde_json::Value>()
                        .await
                        .ok()?;
                    (body["value"]["n"] == serde_json::json!(n)).then_some(())
                })
                .await;
            }
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        for child in self.nodes.iter_mut().flatten() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolling_upgrade_previous_to_current_loses_no_key() {
    let mut g = Group::new();
    for i in 0..3 {
        g.start_node(i, &previous_bin());
    }
    g.wait_leader().await;

    let mut written: Vec<(String, u32)> = Vec::new();
    for k in 0..20u32 {
        let key = format!("up-{k}");
        g.put(&key, k).await;
        written.push((key, k));
    }

    for i in 0..3 {
        g.stop_node(i);
        let key = format!("during-{i}");
        let n = 100 + i as u32;
        g.put(&key, n).await;
        written.push((key, n));

        g.start_node(i, &current_bin());
        g.wait_ready(i).await;
        g.wait_leader().await;

        let key = format!("after-{i}");
        let n = 200 + i as u32;
        g.put(&key, n).await;
        written.push((key, n));
        g.assert_all_keys(&written).await;
    }

    assert_eq!(written.len(), 26);
    g.assert_all_keys(&written).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolling_back_one_replica_loses_no_key() {
    let mut g = Group::new();
    for i in 0..3 {
        g.start_node(i, &current_bin());
    }
    g.wait_leader().await;

    let mut written: Vec<(String, u32)> = Vec::new();
    for k in 0..10u32 {
        let key = format!("base-{k}");
        g.put(&key, k).await;
        written.push((key, k));
    }

    g.stop_node(2);
    g.put("during-rollback", 300).await;
    written.push(("during-rollback".to_string(), 300));

    g.start_node(2, &previous_bin());
    g.wait_ready(2).await;
    g.wait_leader().await;

    g.put("after-rollback", 301).await;
    written.push(("after-rollback".to_string(), 301));
    assert_eq!(written.len(), 12);
    g.assert_all_keys(&written).await;

    // Mixed group: restart a current-build node and keep taking writes.
    g.stop_node(0);
    g.start_node(0, &current_bin());
    g.wait_ready(0).await;

    g.put("after-mixed-restart", 302).await;
    written.push(("after-mixed-restart".to_string(), 302));
    assert_eq!(written.len(), 13);
    g.assert_all_keys(&written).await;
}
