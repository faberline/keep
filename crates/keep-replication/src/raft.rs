//! raft-runtime-backed consensus (HA phase C). keep's engine is wired as a
//! [`raft_runtime::RaftStateMachine`] so writes go through the shared driver
//! (propose → commit → apply); adopting [`RaftHost`] gives keep the proven h2c
//! peer transport + sole-applier read-your-write it previously lacked.
//!
//! - [`KvStateMachine`] is the engine fronting **one shard's** raft group: the
//!   command is a [`WalOp`] (the same type the WAL + recovery already use) and
//!   apply reuses the WAL-replay path ([`RecoveryManager::apply_one`]).
//!   `snapshot`/`restore` ride `dump_values`/`load_values`, filtered to the
//!   shard's keyspace so each per-shard snapshot carries only its own keys.
//! - [`ShardHosts`] runs **one [`RaftHost`] per owned shard** — keep's
//!   distinguishing complexity vs the single-host services (lumen/relay). A
//!   write routes to its key's shard host; each host's peer router mounts under
//!   `/shard/{id}` so the Vote/Append/InstallSnapshot RPCs ride the serve port.
//!
//! This replaces the earlier hand-rolled `RaftKv`/`ShardedRaft` glue (a
//! single-node-only `raft_core` group with no transport); the routing shell
//! ([`ClusterConfig`]) is unchanged. See the keep HA / Raft capability and TD
//! roadmap.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use raft_runtime::{
    AdmissionRefused, FsyncPolicy, HostConfig, Index, Membership, PromotionRefused, RaftHost,
    RaftStateMachine, RaftStore, RemovalRefused, SnapshotPolicy, StateMachineError,
};

/// Raft node id of a shard-group member, re-exported for callers that build
/// membership requests (the admin HTTP API).
pub use raft_runtime::NodeId;
use serde::{Deserialize, Serialize};

use keep_keyspace::engine::KvEngine;
use keep_keyspace::persistence::recovery::RecoveryManager;
use keep_shared_kernel::cluster::ClusterConfig;
use keep_shared_kernel::types::KvValue;
use keep_storage::format::WalOp;

/// How many committed entries a shard's host applies before it captures a
/// snapshot and compacts the raft log (bounds the log; arms InstallSnapshot for
/// a lagging/fresh replica).
const SNAPSHOT_EVERY: u64 = 1024;

/// A shard's keyspace snapshot: the live values whose key hashes to this shard,
/// tagged with the raft index they are current as of (so `restore` can set the
/// applied index, per the [`RaftStateMachine`] contract).
#[derive(Serialize, Deserialize)]
struct ShardSnapshot {
    up_to: Index,
    values: Vec<(String, KvValue)>,
}

/// keep's [`KvEngine`] driven as a [`RaftStateMachine`] for one shard's group.
///
/// Every shard on a node shares the **one** engine; the state machine is scoped
/// to a `shard` so its snapshot/restore touch only that shard's keys (apply only
/// ever receives that shard's ops, because writes are routed by key). `applied`
/// is this shard group's own raft head — each group has an independent log.
pub struct KvStateMachine {
    engine: Arc<KvEngine>,
    cluster: ClusterConfig,
    shard: u32,
    applied: AtomicU64,
}

impl KvStateMachine {
    /// Wrap `engine` as the state machine for `shard` of `cluster`.
    pub fn new(engine: Arc<KvEngine>, cluster: ClusterConfig, shard: u32) -> Arc<Self> {
        Arc::new(Self {
            engine,
            cluster,
            shard,
            applied: AtomicU64::new(0),
        })
    }
}

impl RaftStateMachine for KvStateMachine {
    fn apply(&self, index: Index, command: &[u8]) -> Result<(), StateMachineError> {
        // Decode the committed command and replay it through the exact WAL
        // recovery path. A bad decode / engine error no-ops the entry (logged)
        // but still advances `applied` so the log keeps moving (sole applier).
        match serde_json::from_slice::<WalOp>(command) {
            Ok(op) => {
                if let Err(e) = RecoveryManager::apply_one(&self.engine, &op) {
                    tracing::warn!(index = index.get(), error = %e, "raft: apply_one error (entry no-ops)");
                }
            }
            Err(e) => {
                tracing::warn!(index = index.get(), error = %e, "raft: undecodable command (entry no-ops)")
            }
        }
        self.applied.store(index.get(), Ordering::Release);
        Ok(())
    }

    fn snapshot(&self, writer: &mut dyn std::io::Write) -> Result<(), StateMachineError> {
        let values = self
            .engine
            .dump_values()
            .into_iter()
            .filter(|(k, _)| self.cluster.shard_for(k) == self.shard)
            .collect();
        let bytes = serde_json::to_vec(&ShardSnapshot {
            up_to: self.applied_index(),
            values,
        })
        .map_err(StateMachineError::other)?;
        writer.write_all(&bytes).map_err(StateMachineError::other)?;
        Ok(())
    }

    fn restore(&self, reader: &mut dyn std::io::Read) -> Result<(), StateMachineError> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(StateMachineError::other)?;
        let snap: ShardSnapshot =
            serde_json::from_slice(&bytes).map_err(StateMachineError::other)?;
        self.engine.load_values(snap.values);
        self.applied.store(snap.up_to.get(), Ordering::Release);
        Ok(())
    }

    fn applied_index(&self) -> Index {
        Index::new(self.applied.load(Ordering::Acquire))
    }
}

/// One [`RaftHost`] per owned shard; a write routes to its key's shard host.
pub struct ShardHosts {
    cluster: ClusterConfig,
    hosts: HashMap<u32, Arc<RaftHost>>,
    pub engine: Arc<KvEngine>,
}

impl ShardHosts {
    /// Spawn one host per `cluster.owned_shards()` shard, persisting each group's
    /// hard state under `{data_dir}/raft/shard-{id}`. `replicas_per_shard <= 1`
    /// runs each shard as a sole-voter single-node group (read-your-write, no
    /// peers); `> 1` derives the shard's replica membership + `/shard/{id}` peer
    /// URLs for the shared h2c transport.
    pub async fn new(
        cluster: ClusterConfig,
        engine: Arc<KvEngine>,
        data_dir: &Path,
        replicas_per_shard: u32,
    ) -> Result<Self> {
        Self::with_snapshot_every(
            cluster,
            engine,
            data_dir,
            replicas_per_shard,
            SNAPSHOT_EVERY,
        )
        .await
    }

    /// As [`new`](Self::new), with the snapshot/compaction cadence overridden
    /// (the snapshot-catch-up test drives a small threshold so InstallSnapshot
    /// fires quickly).
    pub async fn with_snapshot_every(
        cluster: ClusterConfig,
        engine: Arc<KvEngine>,
        data_dir: &Path,
        replicas_per_shard: u32,
        snapshot_every: u64,
    ) -> Result<Self> {
        Self::open(
            cluster,
            engine,
            data_dir,
            replicas_per_shard,
            replicas_per_shard,
            snapshot_every,
        )
        .await
    }

    /// As [`new`](Self::new), with `members_per_shard` voters per shard group.
    /// Replicas `>= members_per_shard` start as learners and wait for the leader
    /// to add them.
    pub async fn with_members(
        cluster: ClusterConfig,
        engine: Arc<KvEngine>,
        data_dir: &Path,
        replicas_per_shard: u32,
        members_per_shard: u32,
    ) -> Result<Self> {
        Self::open(
            cluster,
            engine,
            data_dir,
            replicas_per_shard,
            members_per_shard,
            SNAPSHOT_EVERY,
        )
        .await
    }

    async fn open(
        cluster: ClusterConfig,
        engine: Arc<KvEngine>,
        data_dir: &Path,
        replicas_per_shard: u32,
        members_per_shard: u32,
        snapshot_every: u64,
    ) -> Result<Self> {
        let mut hosts = HashMap::new();
        for shard in hosted_shards(&cluster, replicas_per_shard) {
            let (node_id, membership, peers) =
                shard_topology(&cluster, shard, replicas_per_shard, members_per_shard);
            let dir = data_dir.join("raft").join(format!("shard-{shard}"));
            std::fs::create_dir_all(&dir)?;
            let store = RaftStore::open(
                dir.to_str()
                    .ok_or_else(|| anyhow::anyhow!("raft data dir is not valid UTF-8"))?,
                node_id,
                FsyncPolicy::Always,
            )?;
            let sm = KvStateMachine::new(engine.clone(), cluster.clone(), shard);
            let host = RaftHost::spawn(
                node_id,
                membership,
                peers,
                store,
                sm as Arc<dyn RaftStateMachine>,
                HostConfig::default().with_snapshot(SnapshotPolicy::EveryEntries(snapshot_every)),
            );
            hosts.insert(shard, Arc::new(host));
        }
        Ok(Self {
            cluster,
            hosts,
            engine,
        })
    }

    /// Route `op` to the host owning `key`'s shard and propose it through Raft;
    /// resolves with the assigned raft index once **this node's** engine has
    /// applied it (read-your-write). Errors if `key`'s shard is not hosted here.
    pub async fn write(&self, key: &str, op: WalOp) -> Result<Index> {
        let host = self
            .host_for(key)
            .ok_or_else(|| anyhow::anyhow!("shard for key '{key}' not hosted by this node"))?;
        host.propose(serde_json::to_vec(&op)?).await
    }

    /// Merge every shard host's peer router, each nested under `/shard/{id}`, so
    /// the Vote/Append/InstallSnapshot + leader-redirect RPCs ride the service's
    /// h2c serve port. Peer base URLs carry the matching `/shard/{id}` prefix.
    pub fn router(&self) -> Router {
        let mut app = Router::new();
        for (shard, host) in &self.hosts {
            app = app.merge(Router::new().nest(&format!("/shard/{shard}"), host.router()));
        }
        app
    }

    /// Whether this node hosts `key`'s shard group.
    pub fn owns(&self, key: &str) -> bool {
        self.host_for(key).is_some()
    }

    /// Number of per-shard hosts this node runs.
    pub fn host_count(&self) -> usize {
        self.hosts.len()
    }

    /// The host driving `key`'s shard group (for status / direct propose).
    pub fn host_for(&self, key: &str) -> Option<&Arc<RaftHost>> {
        self.hosts.get(&self.cluster.shard_for(key))
    }

    /// The host driving `shard`'s group, if this node hosts that shard.
    pub fn host_for_shard(&self, shard: u32) -> Option<&Arc<RaftHost>> {
        self.hosts.get(&shard)
    }
}

/// Why a membership change was refused. Every variant leaves the group's
/// membership unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipError {
    /// This node is not the leader of the shard group.
    NotLeader,
    /// The named node is not a member (or learner) of the group.
    NotFound(NodeId),
    /// The node has no address registered, so it cannot be admitted.
    Unroutable(NodeId),
    /// The learner has not caught up enough to be promoted.
    NotCaughtUp,
    /// Removing the node would leave the group without a voter.
    LastVoter(NodeId),
    /// Removing the node would drop the group below its fault tolerance.
    WouldLoseTolerance,
    /// The leader itself cannot be removed; transfer leadership first.
    IsLeader(NodeId),
    /// Another membership transition is still in flight.
    InFlight,
}

impl MembershipError {
    /// A stable machine-readable code for this refusal.
    pub fn code(&self) -> &'static str {
        match self {
            MembershipError::NotLeader => "not_leader",
            MembershipError::NotFound(_) => "unknown_node",
            MembershipError::Unroutable(_) => "unroutable_peer",
            MembershipError::NotCaughtUp => "not_caught_up",
            MembershipError::LastVoter(_) => "last_voter",
            MembershipError::WouldLoseTolerance => "would_lose_tolerance",
            MembershipError::IsLeader(_) => "is_leader",
            MembershipError::InFlight => "transition_in_flight",
        }
    }
}

impl std::fmt::Display for MembershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MembershipError::NotLeader => write!(f, "this node is not the shard leader"),
            MembershipError::NotFound(n) => write!(f, "node {n} is not a member of the group"),
            MembershipError::Unroutable(n) => write!(f, "node {n} has no registered address"),
            MembershipError::NotCaughtUp => write!(f, "the learner has not caught up yet"),
            MembershipError::LastVoter(n) => write!(f, "node {n} is the last voter"),
            MembershipError::WouldLoseTolerance => {
                write!(f, "removing the node would drop fault tolerance")
            }
            MembershipError::IsLeader(n) => write!(f, "node {n} is the leader"),
            MembershipError::InFlight => write!(f, "another membership change is in flight"),
        }
    }
}

impl std::error::Error for MembershipError {}

/// Admit `node` to `host`'s group as a learner, reachable at `url`.
///
/// Changes nothing and returns [`MembershipError::InFlight`] when `node` is
/// already a voter or learner. A refused admission leaves no address behind.
pub async fn add_learner(
    host: &RaftHost,
    node: NodeId,
    url: String,
) -> Result<Index, MembershipError> {
    let st = host.status().await;
    if st.committed_voters.contains(&node) || st.learners.contains(&node) {
        return Err(MembershipError::InFlight);
    }
    host.upsert_peer(node, url).await;
    match host.add_learner(node).await {
        Ok(idx) => Ok(idx),
        Err(e) => {
            host.forget_peer(node).await;
            Err(match e {
                AdmissionRefused::NotLeaderOrTransferInFlight => MembershipError::NotLeader,
                AdmissionRefused::Unroutable { .. } => MembershipError::Unroutable(node),
            })
        }
    }
}

/// Promote the caught-up learner `node` of `host`'s group to voter.
pub async fn promote_learner(host: &RaftHost, node: NodeId) -> Result<Index, MembershipError> {
    let st = host.status().await;
    if !st.learners.contains(&node) && !st.committed_voters.contains(&node) {
        return Err(MembershipError::NotFound(node));
    }
    host.promote_learner(node).await.map_err(|e| match e {
        PromotionRefused::NotLeader => MembershipError::NotLeader,
        PromotionRefused::NotCaughtUp { .. } => MembershipError::NotCaughtUp,
        PromotionRefused::TransitionInFlight => MembershipError::InFlight,
    })
}

/// Remove `node` from `host`'s group and forget its address on success.
pub async fn remove_member(host: &RaftHost, node: NodeId) -> Result<Index, MembershipError> {
    let idx = host.remove_member(node).await.map_err(|e| match e {
        RemovalRefused::NotLeader => MembershipError::NotLeader,
        RemovalRefused::TransitionInFlight => MembershipError::InFlight,
        RemovalRefused::NotAMember { target } => MembershipError::NotFound(target),
        RemovalRefused::IsTheLeader { target } => MembershipError::IsLeader(target),
        RemovalRefused::ToleranceWouldDrop { .. } => MembershipError::WouldLoseTolerance,
        RemovalRefused::WouldEmptyVoterSet { target } => MembershipError::LastVoter(target),
    })?;
    host.forget_peer(node).await;
    Ok(idx)
}

fn hosted_shards(cluster: &ClusterConfig, replicas_per_shard: u32) -> Vec<u32> {
    if replicas_per_shard <= 1 {
        return cluster.owned_shards();
    }
    let node_count = cluster.node_count.max(1) as u32;
    let replicas = replicas_per_shard.min(node_count);
    (0..cluster.shard_count)
        .filter(|shard| {
            let owner = cluster.owner_of_shard(*shard) as u32;
            (0..replicas).any(|r| ((owner + r) % node_count) as usize == cluster.node_id)
        })
        .collect()
}

/// Derive a shard group's raft node id, membership, and peer URLs.
///
/// Single-node (`replicas_per_shard <= 1`): a sole voter (`node 0`, no peers) —
/// the group commits locally (read-your-write with no transport). HA (`> 1`):
/// shard `S`'s replicas are the nodes `(owner + r) % node_count` for
/// `r in 0..replicas`; this node's raft id is its replica index `r`, every
/// replica is a voter, and each peer URL is the peer node's base address with a
/// `/shard/{S}` suffix so it matches the nested peer router.
fn shard_topology(
    cluster: &ClusterConfig,
    shard: u32,
    replicas_per_shard: u32,
    members_per_shard: u32,
) -> (NodeId, Membership, HashMap<NodeId, String>) {
    if replicas_per_shard <= 1 {
        return (
            NodeId::new(0),
            Membership::new(vec![NodeId::new(0)], vec![]),
            HashMap::new(),
        );
    }
    let node_count = cluster.node_count.max(1) as u32;
    let replicas = replicas_per_shard.min(node_count);
    let members = members_per_shard.clamp(1, replicas);
    let owner = cluster.owner_of_shard(shard) as u32;
    let mut node_id = NodeId::new(0);
    let mut peers = HashMap::new();
    for r in 0..replicas {
        let node = ((owner + r) % node_count) as usize;
        let rid = NodeId::new(u64::from(r));
        if node == cluster.node_id {
            node_id = rid;
        } else if let Some(base) = cluster.peers.get(node) {
            peers.insert(rid, format!("{}/shard/{shard}", base.trim_end_matches('/')));
        }
    }
    let voters: Vec<NodeId> = (0..u64::from(members)).map(NodeId::new).collect();
    // A replica at or above `members` joins as a learner of its own id and
    // waits for the leader to add it (`add_learner`, then promote).
    let learners = if u64::from(node_id.get()) >= u64::from(members) {
        vec![node_id]
    } else {
        vec![]
    };
    let membership = Membership::new(voters, learners);
    (node_id, membership, peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(node_id: usize) -> ClusterConfig {
        ClusterConfig::from_shared_topology(
            node_id,
            3,
            1,
            vec!["http://a".into(), "http://b".into(), "http://c".into()],
        )
    }

    #[test]
    fn topology_without_members_is_unchanged() {
        let (node_id, membership, peers) = shard_topology(&cluster(2), 0, 3, 3);
        assert_eq!(node_id, NodeId::new(2));
        assert_eq!(
            membership.voters(),
            &[NodeId::new(0), NodeId::new(1), NodeId::new(2)][..]
        );
        assert!(membership.learners().is_empty());
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn topology_pod_below_members_is_voter() {
        let (node_id, membership, peers) = shard_topology(&cluster(1), 0, 3, 2);
        assert_eq!(node_id, NodeId::new(1));
        assert_eq!(membership.voters(), &[NodeId::new(0), NodeId::new(1)][..]);
        assert!(membership.learners().is_empty());
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn topology_pod_at_members_is_learner() {
        let (node_id, membership, peers) = shard_topology(&cluster(2), 0, 3, 2);
        assert_eq!(node_id, NodeId::new(2));
        assert_eq!(membership.voters(), &[NodeId::new(0), NodeId::new(1)][..]);
        assert_eq!(membership.learners(), &[NodeId::new(2)][..]);
        assert_eq!(
            peers.get(&NodeId::new(0)).map(String::as_str),
            Some("http://a/shard/0")
        );
        assert_eq!(
            peers.get(&NodeId::new(1)).map(String::as_str),
            Some("http://b/shard/0")
        );
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn topology_members_clamped() {
        let (_, membership, _) = shard_topology(&cluster(2), 0, 3, 9);
        assert_eq!(
            membership.voters(),
            &[NodeId::new(0), NodeId::new(1), NodeId::new(2)][..]
        );
        assert!(membership.learners().is_empty());

        let (_, membership, _) = shard_topology(&cluster(0), 0, 3, 0);
        assert_eq!(membership.voters(), &[NodeId::new(0)][..]);
        assert!(membership.learners().is_empty());
    }

    #[test]
    fn topology_single_node_ignores_members() {
        let (node_id, membership, peers) = shard_topology(&cluster(0), 0, 1, 1);
        assert_eq!(node_id, NodeId::new(0));
        assert_eq!(membership.voters(), &[NodeId::new(0)][..]);
        assert!(membership.learners().is_empty());
        assert!(peers.is_empty());
    }
}
