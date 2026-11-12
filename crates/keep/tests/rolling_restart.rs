//! Rolling restart of a 3-replica raft group: one replica at a time stops and
//! starts again, and the group keeps serving reads and writes with no key lost.
//! Needs `--features raft`; the helpers are copies of `raft_node.rs` `multi_node`.

#![cfg(feature = "raft")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keep::persistence::format::WalOp;
use keep::raft::KvStateMachine;
use keep::{ClusterConfig, KvEngine, KvKey, KvValue};
use raft_runtime::{
    FsyncPolicy, HostConfig, Membership, RaftHost, RaftStateMachine, RaftStore, SnapshotPolicy,
};
use tokio::net::TcpListener;

/// A fresh, empty temp dir for this test's raft hard-state stores.
fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("keep-raft-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A pre-bound localhost listener + the base URL peers address it by.
async fn bind() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    (l, format!("http://127.0.0.1:{port}"))
}

/// A node in the in-process cluster: its host handle, its own engine, its
/// address, and the `service_http::serve` task driving its peer router (aborted
/// to "stop" it).
struct Node {
    host: Arc<RaftHost>,
    engine: Arc<KvEngine>,
    addr: SocketAddr,
    serve: tokio::task::JoinHandle<()>,
}

/// Spawn a single-shard raft node on a pre-bound listener, serving its peer
/// router (Vote/Append/InstallSnapshot/publish) over localhost h2c.
async fn spawn_on(
    id: u64,
    voters: Vec<u64>,
    peers: HashMap<u64, String>,
    listener: TcpListener,
    dir: &std::path::Path,
    snapshot_every: u64,
) -> Node {
    let engine = Arc::new(KvEngine::with_shards(4));
    let cluster = ClusterConfig::new(0, 1, 1, vec![]);
    let sm = KvStateMachine::new(engine.clone(), cluster, 0);
    std::fs::create_dir_all(dir).unwrap();
    let store = RaftStore::open(dir.to_str().unwrap(), id, FsyncPolicy::Os).unwrap();
    let host = Arc::new(RaftHost::spawn(
        id,
        Membership {
            voters,
            learners: vec![],
        },
        peers,
        store,
        sm as Arc<dyn RaftStateMachine>,
        HostConfig {
            snapshot: SnapshotPolicy::EveryEntries(snapshot_every),
            ..Default::default()
        },
    ));
    let addr = listener.local_addr().unwrap();
    let router = host.router();
    let serve = tokio::spawn(async move {
        service_http::serve(listener, router, std::future::pending::<()>()).await;
    });
    Node {
        host,
        engine,
        addr,
        serve,
    }
}

/// Build the peer map for node `me` out of every node's (id, url).
fn peers_excluding(me: u64, all: &[(u64, String)]) -> HashMap<u64, String> {
    all.iter()
        .filter(|(id, _)| *id != me)
        .map(|(id, url)| (*id, url.clone()))
        .collect()
}

struct Cluster {
    nodes: Vec<Option<Node>>,
    all: Vec<(u64, String)>,
    addrs: Vec<SocketAddr>,
    dir: std::path::PathBuf,
}

/// Stand up a 3-voter single-shard cluster with full peer connectivity. Each
/// `nodes[i]` is `None` while node `i` is stopped.
async fn start_cluster(dir: &std::path::Path) -> Cluster {
    let mut listeners = Vec::new();
    let mut all = Vec::new();
    for id in 0..3u64 {
        let (l, url) = bind().await;
        listeners.push(l);
        all.push((id, url));
    }
    let voters = vec![0u64, 1, 2];
    let mut nodes = Vec::new();
    for (idx, l) in listeners.into_iter().enumerate() {
        let id = idx as u64;
        let node = spawn_on(
            id,
            voters.clone(),
            peers_excluding(id, &all),
            l,
            &dir.join(format!("n{id}")),
            1024,
        )
        .await;
        nodes.push(Some(node));
    }
    let addrs = nodes.iter().flatten().map(|n| n.addr).collect();
    Cluster {
        nodes,
        all,
        addrs,
        dir: dir.to_path_buf(),
    }
}

/// Poll `check` every 25 ms until it is true; panic on timeout. The only
/// waiting code in this file.
async fn eventually<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The index of the first running node that reports leadership.
async fn leader_index(c: &Cluster) -> Option<usize> {
    for (i, slot) in c.nodes.iter().enumerate() {
        if let Some(node) = slot {
            if node.host.is_leader().await {
                return Some(i);
            }
        }
    }
    None
}

/// Propose a SET through the current leader, waiting until one commits it.
async fn propose_set(c: &Cluster, key: &str, value: &str) {
    let c_ref = c;
    eventually(
        "a write to commit",
        Duration::from_secs(30),
        || async move {
            let Some(i) = leader_index(c_ref).await else {
                return false;
            };
            let node = c_ref.nodes[i].as_ref().unwrap();
            let op = serde_json::to_vec(&WalOp::Set {
                key: key.to_string(),
                value: KvValue::String(value.to_string()),
                ttl: None,
            })
            .unwrap();
            node.host.propose(op).await.is_ok()
        },
    )
    .await;
}

fn has(node: &Node, key: &str, value: &str) -> bool {
    node.engine.get(&KvKey::new(key).unwrap()) == Some(KvValue::String(value.to_string()))
}

/// Stop node `i`: abort its peer router, as the `raft_node.rs` crash test does.
async fn stop_node(c: &mut Cluster, i: usize) {
    let node = c.nodes[i].take().unwrap();
    node.serve.abort();
    let _ = node.serve.await;
}

/// Start node `i` again on its old address and its old store directory.
async fn restart_node(c: &mut Cluster, i: usize) {
    let addr = c.addrs[i];
    let slot: std::sync::Mutex<Option<TcpListener>> = std::sync::Mutex::new(None);
    let slot_ref = &slot;
    eventually(
        "the stopped node's port to be free",
        Duration::from_secs(10),
        || async move {
            match TcpListener::bind(addr).await {
                Ok(l) => {
                    *slot_ref.lock().unwrap() = Some(l);
                    true
                }
                Err(_) => false,
            }
        },
    )
    .await;
    let listener = slot.into_inner().unwrap().unwrap();
    let node = spawn_on(
        i as u64,
        vec![0, 1, 2],
        peers_excluding(i as u64, &c.all),
        listener,
        &c.dir.join(format!("n{i}")),
        1024,
    )
    .await;
    c.nodes[i] = Some(node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rolling_restart_keeps_serving_and_loses_no_key() {
    let dir = tmp_dir("rolling3");
    let mut c = start_cluster(&dir).await;

    let c_ref = &c;
    eventually("a leader", Duration::from_secs(20), || async move {
        leader_index(c_ref).await.is_some()
    })
    .await;

    propose_set(&c, "k-start", "v").await;
    let c_ref = &c;
    eventually(
        "k-start everywhere",
        Duration::from_secs(20),
        || async move { c_ref.nodes.iter().flatten().all(|n| has(n, "k-start", "v")) },
    )
    .await;

    let mut expected = vec!["k-start".to_string()];
    for i in 0..3 {
        stop_node(&mut c, i).await;

        // Two of three voters are up, so writes must still commit.
        let key = format!("during-{i}");
        propose_set(&c, &key, "v").await;
        expected.push(key);

        // Every surviving node serves every key.
        let c_ref = &c;
        let keys = &expected;
        eventually(
            "survivors serve all keys",
            Duration::from_secs(20),
            || async move {
                c_ref
                    .nodes
                    .iter()
                    .flatten()
                    .all(|n| keys.iter().all(|k| has(n, k, "v")))
            },
        )
        .await;

        restart_node(&mut c, i).await;

        // The restarted node recovers its log and catches up from the leader.
        let c_ref = &c;
        let keys = &expected;
        eventually(
            "the restarted node has every key",
            Duration::from_secs(30),
            || async move {
                c_ref.nodes[i]
                    .as_ref()
                    .is_some_and(|n| keys.iter().all(|k| has(n, k, "v")))
            },
        )
        .await;
    }

    for n in c.nodes.iter().flatten() {
        for k in &expected {
            assert!(has(n, k, "v"), "{k} is missing on a node");
        }
    }

    propose_set(&c, "k-end", "v").await;
    let c_ref = &c;
    eventually("k-end everywhere", Duration::from_secs(20), || async move {
        c_ref.nodes.iter().flatten().all(|n| has(n, "k-end", "v"))
    })
    .await;

    for n in c.nodes.iter().flatten() {
        n.serve.abort();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
