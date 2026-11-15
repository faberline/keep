//! Dynamic raft membership (keep#30): add a node to a shard group as a learner,
//! promote it to voter, and remove a member, on a real in-process localhost h2c
//! cluster. Only compiles/runs with `--features raft`.

#![cfg(feature = "raft")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keep::persistence::format::WalOp;
use keep::raft::{KvStateMachine, MembershipError};
use keep::{ClusterConfig, KvEngine, KvKey, KvValue};
use raft_runtime::{
    FsyncPolicy, HostConfig, Membership, NodeId, RaftHost, RaftStateMachine, RaftStore,
    SnapshotPolicy,
};
use tokio::net::TcpListener;

const DEADLINE: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(25);

/// A fresh, empty temp dir for a test's raft hard-state stores.
fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("keep-raft-membership-{tag}-{}", std::process::id()));
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

/// A node in the in-process cluster: its host handle, its own engine, its URL,
/// and the `service_http::serve` task driving its peer router.
struct Node {
    id: u64,
    url: String,
    host: Arc<RaftHost>,
    engine: Arc<KvEngine>,
    #[allow(dead_code)]
    serve: tokio::task::JoinHandle<()>,
}

/// Spawn one shard-0 raft node with the given membership on a pre-bound listener.
async fn spawn_on(
    id: u64,
    url: String,
    membership: Membership,
    peers: HashMap<NodeId, String>,
    listener: TcpListener,
    dir: &std::path::Path,
) -> Node {
    let engine = Arc::new(KvEngine::with_shards(4));
    let cluster = ClusterConfig::new(0, 1, 1, vec![]);
    let sm = KvStateMachine::new(engine.clone(), cluster, 0);
    std::fs::create_dir_all(dir).unwrap();
    let store = RaftStore::open(dir.to_str().unwrap(), NodeId::new(id), FsyncPolicy::Os).unwrap();
    let host = Arc::new(RaftHost::spawn(
        NodeId::new(id),
        membership,
        peers,
        store,
        sm as Arc<dyn RaftStateMachine>,
        HostConfig::default().with_snapshot(SnapshotPolicy::EveryEntries(1024)),
    ));
    let router = host.router();
    let serve = tokio::spawn(async move {
        service_http::serve(listener, router, std::future::pending::<()>()).await;
    });
    Node {
        id,
        url,
        host,
        engine,
        serve,
    }
}

/// Stand up an `n`-voter group (ids 0..n) with full peer connectivity.
async fn cluster(n: u64, dir: &std::path::Path) -> Vec<Node> {
    let mut listeners = Vec::new();
    let mut all: Vec<(u64, String)> = Vec::new();
    for id in 0..n {
        let (l, url) = bind().await;
        listeners.push(l);
        all.push((id, url));
    }
    let voters: Vec<NodeId> = (0..n).map(NodeId::new).collect();
    let mut nodes = Vec::new();
    for (idx, l) in listeners.into_iter().enumerate() {
        let id = idx as u64;
        let peers: HashMap<NodeId, String> = all
            .iter()
            .filter(|(p, _)| *p != id)
            .map(|(p, url)| (NodeId::new(*p), url.clone()))
            .collect();
        let url = all[idx].1.clone();
        nodes.push(
            spawn_on(
                id,
                url,
                Membership::new(voters.clone(), vec![]),
                peers,
                l,
                &dir.join(format!("n{id}")),
            )
            .await,
        );
    }
    nodes
}

/// Wait until some node reports leadership, returning its index in `nodes`.
async fn await_leader(nodes: &[Node]) -> usize {
    let deadline = Instant::now() + DEADLINE;
    loop {
        for (i, n) in nodes.iter().enumerate() {
            if n.host.is_leader().await {
                return i;
            }
        }
        assert!(Instant::now() < deadline, "no leader elected within 10 s");
        tokio::time::sleep(TICK).await;
    }
}

/// Wait until `host` reports `id` among its committed voters (`present`) or
/// absent from them (`!present`).
async fn await_voter(host: &RaftHost, id: NodeId, present: bool) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let voters = host.status().await.committed_voters;
        if voters.contains(&id) == present {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for node {id} present={present} in committed voters"
        );
        tokio::time::sleep(TICK).await;
    }
}

/// Wait until `node` has applied at least raft index `target`.
async fn await_node_applied(node: &Node, target: u64) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if node.host.status().await.applied_index >= target {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "node {} did not apply index {target} within 10 s",
            node.id
        );
        tokio::time::sleep(TICK).await;
    }
}

/// Wait until `engine` holds `key` = `value`.
async fn await_value(engine: &KvEngine, key: &str, value: &str) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if engine.get(&KvKey::new(key).unwrap()) == Some(KvValue::String(value.to_string())) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "key {key} did not reach {value} within 10 s"
        );
        tokio::time::sleep(TICK).await;
    }
}

/// Propose a `Set` through `leader` and return its raft index.
async fn set_via(leader: &Node, key: &str, value: &str) -> u64 {
    let cmd = serde_json::to_vec(&WalOp::Set {
        key: key.to_string(),
        value: KvValue::String(value.to_string()),
        ttl: None,
    })
    .unwrap();
    leader.host.propose(cmd).await.unwrap().get()
}

/// Grow a 3-voter group to 4 voters: spawn node 3 as a learner, add it through
/// the leader, wait for it to catch up, then promote it. Returns the leader's
/// index in `nodes` (node 3 is pushed last).
async fn grow_to_four(nodes: &mut Vec<Node>, dir: &std::path::Path) -> usize {
    let leader_idx = await_leader(nodes).await;
    let (l3, url3) = bind().await;
    let voters: Vec<NodeId> = (0..3).map(NodeId::new).collect();
    let peers: HashMap<NodeId, String> = nodes
        .iter()
        .map(|n| (NodeId::new(n.id), n.url.clone()))
        .collect();
    let node3 = spawn_on(
        3,
        url3.clone(),
        Membership::new(voters, vec![NodeId::new(3)]),
        peers,
        l3,
        &dir.join("n3"),
    )
    .await;
    let leader = &nodes[leader_idx];

    let idx = keep::raft::add_learner(&leader.host, NodeId::new(3), url3)
        .await
        .expect("add_learner admits node 3 as a learner");
    nodes.push(node3);

    // Node 3 has the leader's log up to the leader's commit point. This is the
    // log position, not `applied_index`: a membership-only log does not advance
    // the state machine's applied index, so `applied_index` stays 0 on an idle group.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let l = nodes[leader_idx].host.status().await;
        let n3 = nodes[3].host.status().await;
        if n3.commit_index >= l.commit_index.max(idx.get()) && n3.last_index >= l.last_index {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "learner node 3 did not catch up: leader commit={} last={}; node3 commit={} last={} role={}",
            l.commit_index, l.last_index, n3.commit_index, n3.last_index, n3.role
        );
        tokio::time::sleep(TICK).await;
    }

    // Promotion refuses (NotCaughtUp) until the leader sees node 3 matched;
    // retry only that refusal until the deadline.
    loop {
        match keep::raft::promote_learner(&nodes[leader_idx].host, NodeId::new(3)).await {
            Ok(_) => break,
            Err(MembershipError::NotCaughtUp) => {
                assert!(
                    Instant::now() < deadline,
                    "node 3 never caught up for promotion"
                );
                tokio::time::sleep(TICK).await;
            }
            Err(e) => panic!("promote_learner(node 3) refused: {e}"),
        }
    }
    await_voter(&nodes[leader_idx].host, NodeId::new(3), true).await;
    leader_idx
}

/// Add a learner, promote it, and then a write through the leader reaches the
/// new voter's engine.
#[tokio::test]
async fn add_learner_then_promote_replicates_writes() {
    let dir = tmp_dir("add");
    let mut nodes = cluster(3, &dir).await;
    let leader_idx = grow_to_four(&mut nodes, &dir).await;

    set_via(&nodes[leader_idx], "after-add", "replicated").await;
    await_value(&nodes[3].engine, "after-add", "replicated").await;

    let _ = std::fs::remove_dir_all(&dir);
}

/// Remove a non-leader voter from a 4-voter group: the group keeps its quorum,
/// the removed node leaves the committed voters, and writes still commit.
#[tokio::test]
async fn remove_voter_keeps_quorum_and_writes() {
    let dir = tmp_dir("remove");
    let mut nodes = cluster(3, &dir).await;
    let leader_idx = grow_to_four(&mut nodes, &dir).await;

    let victim_idx = (0..3)
        .find(|i| *i != leader_idx)
        .expect("a non-leader voter among the original three");
    let victim = NodeId::new(nodes[victim_idx].id);
    let leader = &nodes[leader_idx];

    keep::raft::remove_member(&leader.host, victim)
        .await
        .expect("remove_member removes a non-leader voter from 4 voters");
    await_voter(&leader.host, victim, false).await;

    let idx = set_via(leader, "after-remove", "quorum").await;
    await_node_applied(leader, idx).await;
    await_value(&leader.engine, "after-remove", "quorum").await;

    let _ = std::fs::remove_dir_all(&dir);
}

/// Refused requests change nothing: unknown nodes give NotFound, an existing
/// voter cannot be added again, and the last voter of a group cannot leave.
#[tokio::test]
async fn refused_requests_change_nothing() {
    let dir = tmp_dir("refused");
    let nodes = cluster(3, &dir).await;
    let leader_idx = await_leader(&nodes).await;
    let leader = &nodes[leader_idx];

    async fn sorted_membership(node: &Node) -> (Vec<NodeId>, Vec<NodeId>) {
        let st = node.host.status().await;
        let mut voters = st.committed_voters;
        let mut learners = st.learners;
        voters.sort();
        learners.sort();
        (voters, learners)
    }

    let before = sorted_membership(leader).await;

    let removed = keep::raft::remove_member(&leader.host, NodeId::new(99)).await;
    assert_eq!(
        removed,
        Err(MembershipError::NotFound(NodeId::new(99))),
        "removing an unknown node is NotFound"
    );
    assert_eq!(
        sorted_membership(leader).await,
        before,
        "membership unchanged after remove_member"
    );

    let promoted = keep::raft::promote_learner(&leader.host, NodeId::new(99)).await;
    assert_eq!(
        promoted,
        Err(MembershipError::NotFound(NodeId::new(99))),
        "promoting an unknown node is NotFound"
    );
    assert_eq!(
        sorted_membership(leader).await,
        before,
        "membership unchanged after promote_learner"
    );

    let added = keep::raft::add_learner(&leader.host, NodeId::new(0), nodes[0].url.clone()).await;
    assert_eq!(
        added,
        Err(MembershipError::InFlight),
        "adding an existing voter is refused"
    );
    assert_eq!(
        sorted_membership(leader).await,
        before,
        "membership unchanged after add_learner"
    );

    // A one-node group cannot lose its only voter.
    let solo_dir = tmp_dir("refused-solo");
    let solo = cluster(1, &solo_dir).await;
    let solo_idx = await_leader(&solo).await;
    let err = keep::raft::remove_member(&solo[solo_idx].host, NodeId::new(0))
        .await
        .expect_err("the last voter cannot be removed");
    assert!(
        err.code() == "last_voter" || err.code() == "is_leader",
        "last voter refusal code, got {}",
        err.code()
    );
    let solo_voters = solo[solo_idx].host.status().await.committed_voters;
    assert!(
        solo_voters.contains(&NodeId::new(0)),
        "the sole voter is still a member"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&solo_dir);
}
