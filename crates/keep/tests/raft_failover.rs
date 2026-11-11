//! Failover of a fixed 3-voter raft group (issue #69).
//!
//! Three replicas run in one process over real localhost h2c sockets. The test
//! writes a key, stops the leader, and proves that a new leader takes writes and
//! keeps the data written before the stop. It then restarts the stopped node on
//! its old raft store and proves that the node catches up and applies new writes.
//! Every wait polls with a timeout; there are no fixed sleeps.
//!
//! Only compiles/runs with `--features raft`.

#![cfg(feature = "raft")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use keep::persistence::format::WalOp;
use keep::raft::KvStateMachine;
use keep::{ClusterConfig, KvEngine, KvKey, KvValue};
use raft_runtime::{
    FsyncPolicy, HostConfig, Membership, RaftHost, RaftStateMachine, RaftStore, SnapshotPolicy,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// A fresh, empty temp dir for a test's raft hard-state stores.
fn tmp_dir(tag: &str) -> PathBuf {
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

/// A node in the in-process cluster: its raft id and address, its host handle,
/// its own engine, and the serve task driving its peer router (aborted to "crash" it).
struct Node {
    id: u64,
    addr: SocketAddr,
    host: Arc<RaftHost>,
    engine: Arc<KvEngine>,
    serve: JoinHandle<()>,
}

/// The whole cluster. `nodes[i]` is `None` while node `i` is stopped.
struct Cluster {
    nodes: Vec<Option<Node>>,
    all: Vec<(u64, String)>,
    addrs: Vec<SocketAddr>,
    dir: PathBuf,
    snapshot_every: u64,
}

/// Spawn one raft node on a pre-bound listener, serving its peer router
/// (Vote/Append/InstallSnapshot/publish) over localhost h2c.
async fn spawn_on(
    id: u64,
    voters: Vec<u64>,
    peers: HashMap<u64, String>,
    listener: TcpListener,
    dir: &Path,
    snapshot_every: u64,
) -> Node {
    let addr = listener.local_addr().unwrap();
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
    let router = host.router();
    let serve = tokio::spawn(async move {
        service_http::serve(listener, router, std::future::pending::<()>()).await;
    });
    Node {
        id,
        addr,
        host,
        engine,
        serve,
    }
}

/// The peer map for node `me`: every other node's (id, url).
fn peers_excluding(me: u64, all: &[(u64, String)]) -> HashMap<u64, String> {
    all.iter()
        .filter(|(id, _)| *id != me)
        .map(|(id, url)| (*id, url.clone()))
        .collect()
}

/// Stand up an `n`-voter single-shard cluster with full peer connectivity:
/// pre-bind every port, then spawn each host with the complete peer map.
async fn start_cluster(n: u64, dir: &Path, snapshot_every: u64) -> Cluster {
    let mut listeners = Vec::new();
    let mut all = Vec::new();
    let mut addrs = Vec::new();
    for id in 0..n {
        let (l, url) = bind().await;
        addrs.push(l.local_addr().unwrap());
        listeners.push(l);
        all.push((id, url));
    }
    let voters: Vec<u64> = (0..n).collect();
    let mut nodes = Vec::new();
    for (idx, l) in listeners.into_iter().enumerate() {
        let id = idx as u64;
        let node = spawn_on(
            id,
            voters.clone(),
            peers_excluding(id, &all),
            l,
            &dir.join(format!("n{id}")),
            snapshot_every,
        )
        .await;
        nodes.push(Some(node));
    }
    Cluster {
        nodes,
        all,
        addrs,
        dir: dir.to_path_buf(),
        snapshot_every,
    }
}

/// Poll `check` until it returns true, or panic when `timeout` passes.
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

/// The index of a running node that reports leadership, if any.
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

/// Propose `key = value` through whichever node leads, retrying until a
/// proposal is accepted. A follower or a leader change makes `propose` fail.
async fn propose_set(c: &Cluster, key: &str, value: &str) {
    let cmd = serde_json::to_vec(&WalOp::Set {
        key: key.to_string(),
        value: KvValue::String(value.to_string()),
        ttl: None,
    })
    .unwrap();
    eventually("a write to commit", Duration::from_secs(20), move || {
        let cmd = cmd.clone();
        async move {
            match leader_index(c).await {
                Some(i) => c.nodes[i].as_ref().unwrap().host.propose(cmd).await.is_ok(),
                None => false,
            }
        }
    })
    .await;
}

/// Whether `node` holds `key = value` in its engine right now.
fn has(node: &Node, key: &str, value: &str) -> bool {
    node.engine.get(&KvKey::new(key).unwrap()) == Some(KvValue::String(value.to_string()))
}

/// Stop node `i` the way a crash looks from outside: abort its serve task and
/// drop its host, so it stops answering peers.
async fn stop_node(c: &mut Cluster, i: usize) {
    let node = c.nodes[i].take().unwrap();
    node.serve.abort();
    let Node {
        serve,
        host,
        engine,
        ..
    } = node;
    let _ = serve.await;
    drop(host);
    drop(engine);
}

/// Restart node `i` on its old address and its old raft store. The state
/// machine is in memory, so the node gets a fresh engine; the host recovers the
/// persisted log and replays the committed entries into it.
async fn restart_node(c: &mut Cluster, i: usize) {
    let addr = c.addrs[i];
    let deadline = Instant::now() + Duration::from_secs(10);
    let listener = loop {
        match TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) => {
                if Instant::now() >= deadline {
                    panic!("timed out waiting for the stopped node's port to be free: {e}");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };
    let voters: Vec<u64> = (0..c.all.len() as u64).collect();
    let node = spawn_on(
        i as u64,
        voters,
        peers_excluding(i as u64, &c.all),
        listener,
        &c.dir.join(format!("n{i}")),
        c.snapshot_every,
    )
    .await;
    assert_eq!(node.id, i as u64);
    assert_eq!(node.addr, addr, "the restarted node keeps its address");
    c.nodes[i] = Some(node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_replicas_survive_the_loss_of_the_leader_and_the_node_rejoins() {
    let dir = tmp_dir("failover3");
    let mut c = start_cluster(3, &dir, 1024).await;

    // 1-2: a leader is elected.
    {
        let cr = &c;
        eventually("a leader", Duration::from_secs(20), || async move {
            leader_index(cr).await.is_some()
        })
        .await;
    }
    let old = leader_index(&c).await.expect("a leader is elected");

    // 3: a write before the stop reaches every node.
    propose_set(&c, "before", "v1").await;
    for i in 0..3 {
        let node = c.nodes[i].as_ref().unwrap();
        eventually(
            &format!("before is replicated to node {i}"),
            Duration::from_secs(20),
            || std::future::ready(has(node, "before", "v1")),
        )
        .await;
    }

    // 4: stop the leader.
    stop_node(&mut c, old).await;

    // 5: a new leader is elected among the survivors.
    {
        let cr = &c;
        eventually("a new leader", Duration::from_secs(20), || async move {
            leader_index(cr).await.is_some()
        })
        .await;
    }
    let new = leader_index(&c).await.expect("a new leader is elected");
    assert_ne!(new, old, "the stopped node cannot lead");

    // 6: the new leader accepts a write, and both keys are on every survivor.
    propose_set(&c, "after", "v2").await;
    for i in (0..3).filter(|&i| i != old) {
        let node = c.nodes[i].as_ref().unwrap();
        eventually(
            &format!("before and after are readable on survivor {i}"),
            Duration::from_secs(20),
            || std::future::ready(has(node, "before", "v1") && has(node, "after", "v2")),
        )
        .await;
    }

    // 7: the stopped node restarts, catches up and has every key.
    restart_node(&mut c, old).await;
    {
        let restarted = c.nodes[old].as_ref().unwrap();
        eventually(
            "the restarted node to have every key",
            Duration::from_secs(30),
            || std::future::ready(has(restarted, "before", "v1") && has(restarted, "after", "v2")),
        )
        .await;
    }

    // 8: the restarted node applies a new write.
    propose_set(&c, "last", "v3").await;
    {
        let restarted = c.nodes[old].as_ref().unwrap();
        eventually(
            "the restarted node to apply a new write",
            Duration::from_secs(20),
            || std::future::ready(has(restarted, "last", "v3")),
        )
        .await;
    }

    for n in c.nodes.iter().flatten() {
        n.serve.abort();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
