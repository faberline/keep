//! Raft failover gate (HA phase C) against **real OS processes**.
//!
//! Each test boots a 3-replica shard group of the compiled `keep` binary on
//! localhost, configured the way the operator configures a StatefulSet pod
//! (`POD_NAME`/`SHARD_COUNT`/`REPLICAS_PER_SHARD`/`VOTER_COUNT`, with
//! `KEEP_PEERS` mapping replica ids to local ports). Crashes are real signals:
//! SIGKILL for a crash, SIGSTOP/SIGCONT for a hung or partitioned leader,
//! SIGTERM for a rolling restart. In-process clusters cannot model these: a
//! dropped tokio task does not lose its unflushed state or its open sockets.
//!
//! The tests share one lock so their election timing does not compete for
//! CPU; run them with `--test-threads=1` for clean logs either way.

#![cfg(all(feature = "raft", unix))]

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// One test cluster at a time: elections are timing-sensitive.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const REPLICAS: usize = 3;
const WAIT: Duration = Duration::from_secs(20);

/// Environment a developer shell might carry that would change placement.
const TOPOLOGY_ENV: &[&str] = &[
    "KEEP_NODE_ID",
    "KEEP_NODE_COUNT",
    "KEEP_SHARD_COUNT",
    "KEEP_REPLICAS_PER_SHARD",
    "KEEP_HEADLESS_SERVICE",
    "KEEP_DATA_DIR",
    "KEEP_API_TOKEN",
    "KEEP_TOKEN_SECRET",
    "KEEP_MAXMEMORY",
    "RUST_LOG",
];

struct Node {
    port: u16,
    dir: PathBuf,
    child: Option<Child>,
    log: Arc<Mutex<Vec<String>>>,
}

struct Cluster {
    root: PathBuf,
    ports: Vec<u16>,
    snapshot_every: u64,
    /// Replicas `0..voters` vote; the rest are learners.
    voters: usize,
    nodes: Vec<Node>,
    http: reqwest::Client,
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Cluster {
    fn start(tag: &str, snapshot_every: u64) -> Self {
        Self::start_with_voters(tag, snapshot_every, REPLICAS)
    }

    fn start_with_voters(tag: &str, snapshot_every: u64, voters: usize) -> Self {
        let root =
            std::env::temp_dir().join(format!("keep-raft-proc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ports: Vec<u16> = (0..REPLICAS).map(|_| free_port()).collect();
        let mut cluster = Self {
            nodes: Vec::new(),
            root,
            ports,
            snapshot_every,
            voters,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        };
        for i in 0..REPLICAS {
            let dir = cluster.root.join(format!("keep-{i}"));
            cluster.nodes.push(Node {
                port: cluster.ports[i],
                dir,
                child: None,
                log: Arc::new(Mutex::new(Vec::new())),
            });
            cluster.spawn(i);
        }
        cluster
    }

    /// (Re)start replica `i` on its port and data dir.
    fn spawn(&mut self, i: usize) {
        let peers = self
            .ports
            .iter()
            .map(|p| format!("127.0.0.1:{p}"))
            .collect::<Vec<_>>()
            .join(",");
        let node = &mut self.nodes[i];
        std::fs::create_dir_all(&node.dir).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_keep"));
        for key in TOPOLOGY_ENV {
            cmd.env_remove(key);
        }
        let mut child = cmd
            .args(["--host", "127.0.0.1", "--port", &node.port.to_string()])
            .arg("--data-dir")
            .arg(&node.dir)
            .args(["--grace-secs", "0", "--log-format", "json"])
            .args(["--raft-snapshot-every", &self.snapshot_every.to_string()])
            .env("POD_NAME", format!("keep-{i}"))
            .env("SHARD_COUNT", "1")
            .env("REPLICAS_PER_SHARD", REPLICAS.to_string())
            .env("VOTER_COUNT", self.voters.to_string())
            .env("KEEP_PEERS", &peers)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn keep replica");
        for pipe in [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let log = node.log.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    log.lock().unwrap().push(line);
                }
            });
        }
        node.child = Some(child);
    }

    fn pid(&self, i: usize) -> u32 {
        self.nodes[i].child.as_ref().expect("replica running").id()
    }

    fn signal(&self, i: usize, sig: &str) {
        let status = Command::new("kill")
            .args([format!("-{sig}"), self.pid(i).to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "kill -{sig} replica {i}");
    }

    /// SIGKILL replica `i` and reap it.
    fn crash(&mut self, i: usize) {
        if let Some(mut child) = self.nodes[i].child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn url(&self, i: usize, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.ports[i])
    }

    fn log_text(&self, i: usize) -> String {
        self.nodes[i].log.lock().unwrap().join("\n")
    }

    async fn status(&self, i: usize) -> Option<Value> {
        let resp = self
            .http
            .get(self.url(i, "/shard/0/raftz"))
            .send()
            .await
            .ok()?;
        resp.json().await.ok()
    }

    /// Wait for a leader among `among` whose term is above `min_term`; returns
    /// (replica, term).
    async fn leader(&self, among: &[usize], min_term: u64) -> (usize, u64) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            for &i in among {
                if let Some(st) = self.status(i).await {
                    let term = st["term"].as_u64().unwrap_or(0);
                    if st["is_leader"] == json!(true) && term > min_term {
                        return (i, term);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no leader among {among:?} above term {min_term}");
    }

    async fn put(&self, i: usize, key: &str, body: Value) -> reqwest::StatusCode {
        self.http
            .put(self.url(i, &format!("/kv/{key}")))
            .json(&body)
            .send()
            .await
            .map(|r| r.status())
            .unwrap_or(reqwest::StatusCode::SERVICE_UNAVAILABLE)
    }

    async fn post(&self, i: usize, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let resp = self
            .http
            .post(self.url(i, path))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn delete(&self, i: usize, key: &str) -> reqwest::StatusCode {
        self.http
            .delete(self.url(i, &format!("/kv/{key}")))
            .send()
            .await
            .unwrap()
            .status()
    }

    /// Replica `i`'s local view of `key` (`None` = 404).
    /// GET `key` on replica `i` at read-consistency `level`.
    async fn read(&self, i: usize, key: &str, level: &str) -> (reqwest::StatusCode, Value) {
        let resp = self
            .http
            .get(self.url(i, &format!("/kv/{key}")))
            .header("x-read-consistency", level)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// Linearizable read of `key` through replica `i` (`None` = 404).
    async fn get(&self, i: usize, key: &str) -> Option<Value> {
        let (status, body) = self.read(i, key, "leader").await;
        match status {
            reqwest::StatusCode::OK => Some(body["value"].clone()),
            reqwest::StatusCode::NOT_FOUND => None,
            other => panic!("read {key} on replica {i}: {other} {body}"),
        }
    }

    /// Wait until replica `i` has applied everything replica `leader` has
    /// committed.
    async fn caught_up(&self, i: usize, leader: usize) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let (Some(me), Some(lead)) = (self.status(i).await, self.status(leader).await) {
                let applied = me["applied_index"].as_u64().unwrap_or(0);
                if applied > 0 && applied >= lead["commit_index"].as_u64().unwrap_or(u64::MAX) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "replica {i} did not catch up to leader {leader}:\n{}",
            self.log_text(i)
        );
    }

    async fn put_many(&self, via: usize, prefix: &str, n: usize) {
        for k in 0..n {
            let status = self
                .put(via, &format!("{prefix}{k}"), json!({ "value": k }))
                .await;
            assert_eq!(status, reqwest::StatusCode::OK, "write {prefix}{k}");
        }
    }

    /// Replica `i` serves every `{prefix}{k}`. The first read fences (so the
    /// replica is current); the rest read its local state directly.
    async fn assert_has_many(&self, i: usize, prefix: &str, n: usize) {
        assert_eq!(self.get(i, &format!("{prefix}0")).await, Some(json!(0)));
        for k in 1..n {
            let (status, body) = self.read(i, &format!("{prefix}{k}"), "any").await;
            assert_eq!(
                (status, &body["value"]),
                (reqwest::StatusCode::OK, &json!(k)),
                "replica {i} lost committed key {prefix}{k}"
            );
        }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for i in 0..self.nodes.len() {
            if self.nodes[i].child.is_some() {
                // A SIGSTOPped process ignores SIGKILL until resumed on some
                // platforms' process accounting; resume it first.
                let _ = Command::new("kill")
                    .args(["-CONT", &self.pid(i).to_string()])
                    .status();
            }
            self.crash(i);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wipe(dir: &Path) {
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::create_dir_all(dir).unwrap();
}

/// A crashed leader loses nothing that was acknowledged; the survivors elect a
/// successor that serves every committed write, and the old leader rejoins
/// from its own log without applying any entry twice.
#[tokio::test(flavor = "multi_thread")]
async fn sigkilled_leader_keeps_committed_writes() {
    let _serial = SERIAL.lock().await;
    let mut c = Cluster::start("sigkill", 1024);
    let (leader, term) = c.leader(&[0, 1, 2], 0).await;

    c.put_many(leader, "k", 40).await;
    for _ in 0..5 {
        let (status, _) = c.post(leader, "/kv/ctr/incr", json!({"delta": 1})).await;
        assert_eq!(status, reqwest::StatusCode::OK);
    }

    c.crash(leader);
    let survivors: Vec<usize> = (0..REPLICAS).filter(|&i| i != leader).collect();
    let (next, _) = c.leader(&survivors, term).await;
    c.assert_has_many(next, "k", 40).await;
    assert_eq!(c.get(next, "ctr").await, Some(json!(5)));
    assert_eq!(
        c.put(next, "after", json!({"value": "failover"})).await,
        reqwest::StatusCode::OK
    );

    c.spawn(leader);
    c.caught_up(leader, next).await;
    c.assert_has_many(leader, "k", 40).await;
    assert_eq!(c.get(leader, "after").await, Some(json!("failover")));
    assert_eq!(
        c.get(leader, "ctr").await,
        Some(json!(5)),
        "restart must rebuild from the raft log once, not WAL + log"
    );
}

/// A replica whose data dir is lost rejoins empty and catches up through
/// InstallSnapshot (the leader has compacted past the entries it needs),
/// including the remaining TTL of expiring keys.
#[tokio::test(flavor = "multi_thread")]
async fn wiped_follower_catches_up_via_install_snapshot() {
    let _serial = SERIAL.lock().await;
    let mut c = Cluster::start("wipe", 16);
    let (leader, _) = c.leader(&[0, 1, 2], 0).await;
    let follower = (leader + 1) % REPLICAS;

    c.put_many(leader, "a", 40).await;
    let ttl = json!({"value": "short-lived", "ttl_ms": 600_000});
    assert_eq!(c.put(leader, "ttl", ttl).await, reqwest::StatusCode::OK);

    c.crash(follower);
    wipe(&c.nodes[follower].dir);
    c.put_many(leader, "b", 40).await;

    c.spawn(follower);
    c.caught_up(follower, leader).await;
    let st = c.status(follower).await.unwrap();
    assert!(
        st["snapshot_index"].as_u64().unwrap_or(0) > 0,
        "catch-up should have installed a snapshot: {st}"
    );
    c.assert_has_many(follower, "a", 40).await;
    c.assert_has_many(follower, "b", 40).await;
    let resp: Value = c
        .http
        .get(c.url(follower, "/kv/ttl/ttl"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ttl_ms = resp["ttl_ms"].as_i64().unwrap();
    assert!(
        (1..=600_000).contains(&ttl_ms),
        "snapshot must carry the key's remaining TTL, got {ttl_ms}"
    );
}

/// A replica that restarts behind a compaction point replaces its state with
/// the leader's snapshot: a key deleted while it was down must not survive.
#[tokio::test(flavor = "multi_thread")]
async fn restarted_follower_snapshot_drops_deleted_keys() {
    let _serial = SERIAL.lock().await;
    let mut c = Cluster::start("stale", 16);
    let (leader, _) = c.leader(&[0, 1, 2], 0).await;
    let follower = (leader + 1) % REPLICAS;

    assert_eq!(
        c.put(leader, "stale", json!({"value": 1})).await,
        reqwest::StatusCode::OK
    );
    c.caught_up(follower, leader).await;
    c.crash(follower);

    assert_eq!(c.delete(leader, "stale").await, reqwest::StatusCode::OK);
    c.put_many(leader, "c", 60).await;

    c.spawn(follower);
    c.caught_up(follower, leader).await;
    c.assert_has_many(follower, "c", 60).await;
    assert_eq!(
        c.get(follower, "stale").await,
        None,
        "snapshot restore must clear keys the leader no longer has"
    );
}

/// A hung leader (SIGSTOP) is deposed: the others elect a successor that takes
/// writes, and once resumed the old leader steps down and converges instead
/// of acting as a second leader.
#[tokio::test(flavor = "multi_thread")]
async fn stopped_leader_is_deposed_and_converges() {
    let _serial = SERIAL.lock().await;
    let c = Cluster::start("sigstop", 1024);
    let (leader, term) = c.leader(&[0, 1, 2], 0).await;
    assert_eq!(
        c.put(leader, "x", json!({"value": "old"})).await,
        reqwest::StatusCode::OK
    );

    c.signal(leader, "STOP");
    let survivors: Vec<usize> = (0..REPLICAS).filter(|&i| i != leader).collect();
    let (next, new_term) = c.leader(&survivors, term).await;
    assert_eq!(
        c.put(next, "x", json!({"value": "new"})).await,
        reqwest::StatusCode::OK
    );
    c.signal(leader, "CONT");
    // The resumed node still believes it leads term `term`; a default read
    // through it must not return the value it held before it was deposed.
    let (status, body) = c.read(leader, "x", "leader").await;
    match status {
        reqwest::StatusCode::OK => assert_eq!(body["value"], json!("new"), "stale read"),
        reqwest::StatusCode::SERVICE_UNAVAILABLE => {}
        other => panic!("unexpected {other}: {body}"),
    }

    let deadline = Instant::now() + WAIT;
    loop {
        let st = c.status(leader).await;
        if let Some(st) = st {
            if st["is_leader"] == json!(false) && st["term"].as_u64().unwrap_or(0) >= new_term {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "resumed leader never stepped down"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    c.caught_up(leader, next).await;
    assert_eq!(c.get(leader, "x").await, Some(json!("new")));
    // A write through the deposed node is forwarded to the real leader.
    assert_eq!(
        c.put(leader, "y", json!({"value": 1})).await,
        reqwest::StatusCode::OK
    );
    c.caught_up(next, leader).await;
    assert_eq!(c.get(next, "y").await, Some(json!(1)));
}

/// Without a quorum a default read fails closed instead of serving state the
/// node cannot prove current; `any` still answers from the local replica.
#[tokio::test(flavor = "multi_thread")]
async fn reads_fail_closed_without_quorum() {
    let _serial = SERIAL.lock().await;
    let c = Cluster::start("quorum", 1024);
    let (leader, _) = c.leader(&[0, 1, 2], 0).await;
    let followers: Vec<usize> = (0..REPLICAS).filter(|&i| i != leader).collect();
    assert_eq!(
        c.put(leader, "q", json!({"value": 1})).await,
        reqwest::StatusCode::OK
    );

    for &f in &followers {
        c.signal(f, "STOP");
    }
    let (status, body) = c.read(leader, "q", "leader").await;
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], json!("read_fence_failed"));
    let (status, body) = c.read(leader, "q", "any").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["value"], json!(1));
    for &f in &followers {
        c.signal(f, "CONT");
    }

    let deadline = Instant::now() + WAIT;
    loop {
        let (status, body) = c.read(leader, "q", "leader").await;
        if status == reqwest::StatusCode::OK {
            assert_eq!(body["value"], json!(1));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reads never recovered: {status} {body}"
        );
    }
}

/// Phase B async replicas are raft learners (`VOTER_COUNT < REPLICAS_PER_SHARD`):
/// a learner receives every committed entry and serves reads at any level, but
/// never leads and never counts toward quorum — stopping it does not slow
/// writes, while stopping the other voter fences reads even though the learner
/// is up.
#[tokio::test(flavor = "multi_thread")]
async fn learner_replicates_without_joining_quorum() {
    let _serial = SERIAL.lock().await;
    let c = Cluster::start_with_voters("learner", 1024, 2);
    let learner = 2;
    let (leader, _) = c.leader(&[0, 1], 0).await;
    let voter = 1 - leader;

    // Writes through the learner are forwarded to the leader.
    c.put_many(learner, "l", 10).await;
    c.caught_up(learner, leader).await;
    c.assert_has_many(learner, "l", 10).await;

    let (status, _) = c.read(learner, "l3", "bounded(60000)").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let cluster: Value = c
        .http
        .get(c.url(learner, "/cluster"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let shard = &cluster["shards"][0];
    assert_eq!(shard["role"], json!("learner"), "{cluster}");
    assert_eq!(shard["voters"], json!([0, 1]));
    assert_eq!(shard["learners"], json!([2]));
    assert_eq!(shard["leader"], json!(leader));

    // The voters alone are a quorum: a stopped learner does not block writes.
    c.signal(learner, "STOP");
    c.put_many(leader, "m", 5).await;
    assert_eq!(c.get(leader, "m4").await, Some(json!(4)));
    c.signal(learner, "CONT");
    c.caught_up(learner, leader).await;
    c.assert_has_many(learner, "m", 5).await;

    // A learner is no substitute for a voter: without the other voter the
    // leader cannot prove a read, and the learner never takes over.
    c.signal(voter, "STOP");
    let (status, body) = c.read(leader, "l0", "leader").await;
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let st = c.status(learner).await.expect("learner status");
    assert_eq!(st["is_leader"], json!(false), "a learner must never lead");
    c.signal(voter, "CONT");
}

/// SIGTERM on the leader hands leadership to a caught-up voter before the
/// process exits, so the group does not sit out an election timeout.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_leader_hands_off_leadership() {
    let _serial = SERIAL.lock().await;
    let mut c = Cluster::start("sigterm", 1024);
    let (leader, term) = c.leader(&[0, 1, 2], 0).await;
    c.put_many(leader, "t", 10).await;

    c.signal(leader, "TERM");
    let mut child = c.nodes[leader].child.take().unwrap();
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "keep did not exit on SIGTERM");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(status.success(), "graceful exit, got {status}");
    let log = c.log_text(leader);
    assert!(
        log.contains("Transferred"),
        "leader should report a leadership transfer on SIGTERM:\n{log}"
    );

    let survivors: Vec<usize> = (0..REPLICAS).filter(|&i| i != leader).collect();
    let (next, _) = c.leader(&survivors, term).await;
    c.assert_has_many(next, "t", 10).await;
}

/// Entries a leader appended but never committed (its followers were
/// unreachable) are truncated when it rejoins under a newer leader; the client
/// never got a 200 for them, and they never become visible.
///
/// Timing-sensitive (relies on SIGSTOP isolating both followers while the
/// leader appends), so it runs in the nightly gate:
/// `cargo test -p keep --test raft_cluster_process -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing-sensitive; nightly failover gate"]
async fn uncommitted_tail_is_truncated_on_rejoin() {
    let _serial = SERIAL.lock().await;
    let mut c = Cluster::start("tail", 1024);
    let (leader, term) = c.leader(&[0, 1, 2], 0).await;
    let followers: Vec<usize> = (0..REPLICAS).filter(|&i| i != leader).collect();
    assert_eq!(
        c.put(leader, "base", json!({"value": 1})).await,
        reqwest::StatusCode::OK
    );
    for &f in &followers {
        c.caught_up(f, leader).await;
    }

    for &f in &followers {
        c.signal(f, "STOP");
    }
    // The leader appends but cannot commit; the client times out, no 200.
    let ghost = c
        .http
        .put(c.url(leader, "/kv/ghost"))
        .timeout(Duration::from_millis(500))
        .json(&json!({"value": "uncommitted"}))
        .send()
        .await;
    assert!(
        ghost.map(|r| !r.status().is_success()).unwrap_or(true),
        "an uncommitted write must not be acknowledged"
    );
    c.crash(leader);
    for &f in &followers {
        c.signal(f, "CONT");
    }

    let (next, _) = c.leader(&followers, term).await;
    assert_eq!(
        c.put(next, "base", json!({"value": 2})).await,
        reqwest::StatusCode::OK
    );
    c.spawn(leader);
    c.caught_up(leader, next).await;
    assert_eq!(c.get(leader, "base").await, Some(json!(2)));
    assert_eq!(
        c.get(leader, "ghost").await,
        None,
        "uncommitted tail survived"
    );
    for &f in &followers {
        assert_eq!(c.get(f, "ghost").await, None);
    }
}
