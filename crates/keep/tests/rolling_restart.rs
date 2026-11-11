//! Rolling restart of a 3-replica raft group (issue #71).
//!
//! The replicas are restarted one at a time. While one is down the group keeps
//! serving reads and writes, and after each restart the node has every key. No
//! key is lost. Every wait polls with a timeout; there are no fixed sleeps.
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

const VOTERS: u64 = 3;
const SNAPSHOT_EVERY: u64 = 1024;

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

/// A node in the in-process cluster: its address, its host handle, its own
/// engine, and the serve task driving its peer router (aborted to "stop" it).
struct Node {
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
}

/// Spawn one raft node on a pre-bound listener, serving its peer router
/// (Vote/Append/InstallSnapshot/publish) over localhost h2c.
async fn spawn_on(
    id: u64,
    voters: Vec<u64>,
    peers: HashMap<u64, String>,
    listener: TcpListener,
    dir: &Path,
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
            snapshot: SnapshotPolicy::EveryEntries(SNAPSHOT_EVERY),
            ..Default::default()
        },
    ));
    let router = host.router();
    let serve = tokio::spawn(async move {
        service_http::serve(listener, router, std::future::pending::<()>()).await;
    });
    Node {
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

/// Stand up a 3-voter single-shard cluster with full peer connectivity.
async fn start_cluster(dir: &Path) -> Cluster {
    let mut listeners = Vec::new();
    let mut all = Vec::new();
    let mut addrs = Vec::new();
    for id in 0..VOTERS {
        let (l, url) = bind().await;
        addrs.push(l.local_addr().unwrap());
        listeners.push(l);
        all.push((id, url));
    }
    let voters: Vec<u64> = (0..VOTERS).collect();
    let mut nodes = Vec::new();
    for (idx, l) in listeners.into_iter().enumerate() {
        let id = idx as u64;
        let node = spawn_on(
            id,
            voters.clone(),
            peers_excluding(id, &all),
            l,
            &dir.join(format!("n{id}")),
        )
        .await;
        nodes.push(Some(node));
    }
    Cluster {
        nodes,
        all,
        addrs,
        dir: dir.to_path_buf(),
    }
}

/// Poll `check` every 25 ms until it returns true, or panic when `timeout` passes.
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

/// Propose `key = value` through the leader, retrying until a proposal is accepted.
async fn propose_set(c: &Cluster, key: &str, value: &str) {
    let cmd = serde_json::to_vec(&WalOp::Set {
        key: key.to_string(),
        value: KvValue::String(value.to_string()),
        ttl: None,
    })
    .unwrap();
    eventually("a write to commit", Duration::from_secs(30), move || {
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

/// Whether `node` holds every key in `keys` with the value "v".
fn all_keys_on(node: &Node, keys: &[String]) -> bool {
    keys.iter().all(|k| has(node, k, "v"))
}

/// Stop node `i` the way a stop looks from outside: abort its serve task and
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
/// persisted log from the same store directory and the leader brings it up to date.
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
    let voters: Vec<u64> = (0..VOTERS).collect();
    let node = spawn_on(
        i as u64,
        voters,
        peers_excluding(i as u64, &c.all),
        listener,
        &c.dir.join(format!("n{i}")),
    )
    .await;
    assert_eq!(node.addr, addr, "the restarted node keeps its address");
    c.nodes[i] = Some(node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rolling_restart_keeps_serving_and_loses_no_key() {
    let dir = tmp_dir("rolling3");
    let mut c = start_cluster(&dir).await;

    {
        let cr = &c;
        eventually("a leader", Duration::from_secs(20), || async move {
            leader_index(cr).await.is_some()
        })
        .await;
    }

    // Step 2: a first key reaches every node.
    propose_set(&c, "k-start", "v").await;
    for i in 0..VOTERS as usize {
        let node = c.nodes[i].as_ref().unwrap();
        eventually(
            &format!("k-start is on node {i}"),
            Duration::from_secs(20),
            || std::future::ready(has(node, "k-start", "v")),
        )
        .await;
    }

    // Step 3: restart the nodes one at a time.
    let mut expected = vec!["k-start".to_string()];
    for i in 0..VOTERS as usize {
        // 3a: stop node i.
        stop_node(&mut c, i).await;

        // 3b: with two of three voters up, writes still commit.
        let key = format!("during-{i}");
        propose_set(&c, &key, "v").await;
        expected.push(key);

        // 3c: every surviving node serves every key, so reads continue.
        for node in c.nodes.iter().flatten() {
            eventually("survivors serve all keys", Duration::from_secs(20), || {
                std::future::ready(all_keys_on(node, &expected))
            })
            .await;
        }

        // 3d: node i comes back and has every key.
        restart_node(&mut c, i).await;
        let restarted = c.nodes[i].as_ref().unwrap();
        eventually(
            &format!("the restarted node {i} has every key"),
            Duration::from_secs(30),
            || std::future::ready(all_keys_on(restarted, &expected)),
        )
        .await;
    }

    // Step 4: no key is lost on any node, and a new write reaches every node.
    for node in c.nodes.iter().flatten() {
        assert!(all_keys_on(node, &expected), "a node lost a key");
    }
    propose_set(&c, "k-end", "v").await;
    for node in c.nodes.iter().flatten() {
        eventually("k-end is on every node", Duration::from_secs(20), || {
            std::future::ready(has(node, "k-end", "v"))
        })
        .await;
    }

    // Step 5: clean up.
    for n in c.nodes.iter().flatten() {
        n.serve.abort();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
