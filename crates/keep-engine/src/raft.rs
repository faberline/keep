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
    FsyncPolicy, GroupId, HostConfig, Index, Membership, NodeId, RaftHost, RaftStateMachine,
    RaftStore, SnapshotPolicy,
};
use serde::{Deserialize, Serialize};

use crate::cluster::ClusterConfig;
use crate::engine::KvEngine;
use crate::persistence::format::WalOp;
use crate::persistence::recovery::RecoveryManager;
use crate::types::KvValue;

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
    fn apply(&self, index: Index, command: &[u8]) -> Result<()> {
        // Decode the committed command and replay it through the exact WAL
        // recovery path. A bad decode / engine error no-ops the entry (logged)
        // but still advances `applied` so the log keeps moving (sole applier).
        match serde_json::from_slice::<WalOp>(command) {
            Ok(op) => {
                if let Err(e) = RecoveryManager::apply_one(&self.engine, &op) {
                    tracing::warn!(index, error = %e, "raft: apply_one error (entry no-ops)");
                }
            }
            Err(e) => {
                tracing::warn!(index, error = %e, "raft: undecodable command (entry no-ops)")
            }
        }
        self.applied.store(index, Ordering::Release);
        Ok(())
    }

    fn snapshot(&self, writer: &mut dyn std::io::Write) -> Result<()> {
        let values = self
            .engine
            .dump_values()
            .into_iter()
            .filter(|(k, _)| self.cluster.shard_for(k) == self.shard)
            .collect();
        let bytes = serde_json::to_vec(&ShardSnapshot {
            up_to: self.applied_index(),
            values,
        })?;
        writer.write_all(&bytes)?;
        Ok(())
    }

    fn restore(&self, reader: &mut dyn std::io::Read) -> Result<()> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        let snap: ShardSnapshot = serde_json::from_slice(&bytes)?;
        self.engine.load_values(snap.values);
        self.applied.store(snap.up_to, Ordering::Release);
        Ok(())
    }

    fn applied_index(&self) -> Index {
        self.applied.load(Ordering::Acquire)
    }
}

/// One [`RaftHost`] per owned shard; a write routes to its key's shard host.
pub struct ShardHosts {
    cluster: ClusterConfig,
    hosts: HashMap<u32, Arc<RaftHost>>,
    pub engine: Arc<KvEngine>,
}

impl ShardHosts {
    /// Spawn the shard hosts this pod runs, persisting each group's hard state
    /// under `{data_dir}/raft/shard-{id}`.
    ///
    /// - Replica mode (`cluster.replica` set): **one** host, for this pod's own
    ///   shard group. Its raft id is the pod's replica index, voters are
    ///   `0..voter_count` and the rest learners (raft-runtime's `ClusterDims`
    ///   model); peers are the group's other replicas with a `/shard/{id}`
    ///   suffix for the shared h2c transport.
    /// - Otherwise: a sole-voter single-node group per owned shard
    ///   (read-your-write, no peers).
    pub async fn new(
        cluster: ClusterConfig,
        engine: Arc<KvEngine>,
        data_dir: &Path,
    ) -> Result<Self> {
        Self::with_snapshot_every(cluster, engine, data_dir, SNAPSHOT_EVERY).await
    }

    /// As [`new`](Self::new), with the snapshot/compaction cadence overridden
    /// (the snapshot-catch-up test drives a small threshold so InstallSnapshot
    /// fires quickly).
    pub async fn with_snapshot_every(
        cluster: ClusterConfig,
        engine: Arc<KvEngine>,
        data_dir: &Path,
        snapshot_every: u64,
    ) -> Result<Self> {
        let mut hosts = HashMap::new();
        for shard in cluster.owned_shards() {
            let topo = shard_topology(&cluster, shard)?;
            let dir = data_dir.join("raft").join(format!("shard-{shard}"));
            std::fs::create_dir_all(&dir)?;
            let dir = dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("raft data dir is not valid UTF-8"))?;
            let sm = KvStateMachine::new(engine.clone(), cluster.clone(), shard);
            let cfg = HostConfig {
                snapshot: SnapshotPolicy::EveryEntries(snapshot_every),
                ..Default::default()
            };
            let host = match topo.group {
                // Named group: peers reject RPCs addressed to another shard's
                // group, so a misrouted `/shard/{id}` URL can never cross-vote.
                Some(group) => {
                    let store = RaftStore::open_group(
                        dir,
                        topo.node_id,
                        group.clone(),
                        FsyncPolicy::Always,
                    )?;
                    RaftHost::spawn_group(
                        topo.node_id,
                        group,
                        topo.membership,
                        topo.peers,
                        store,
                        sm as Arc<dyn RaftStateMachine>,
                        cfg,
                    )
                }
                None => {
                    let store = RaftStore::open(dir, topo.node_id, FsyncPolicy::Always)?;
                    RaftHost::spawn(
                        topo.node_id,
                        topo.membership,
                        topo.peers,
                        store,
                        sm as Arc<dyn RaftStateMachine>,
                        cfg,
                    )
                }
            };
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
}

/// A shard group's raft identity on this pod.
struct ShardTopology {
    node_id: NodeId,
    membership: Membership,
    peers: HashMap<NodeId, String>,
    /// Named raft group (replica mode); `None` keeps the legacy single-node
    /// store layout.
    group: Option<GroupId>,
}

/// Raft group id for a shard's replica group.
pub fn shard_group_id(shard: u32) -> GroupId {
    GroupId(format!("keep-shard-{shard}"))
}

/// Derive a shard group's raft node id, membership, and peer URLs.
///
/// Without a replica placement: a sole voter (`node 0`, no peers) — the group
/// commits locally. With one: this pod is replica `replica_index` of its shard
/// group, voters are `0..voter_count`, learners `voter_count..replicas`, and
/// `cluster.peers[r]` (indexed by replica id) carries each peer's base address,
/// suffixed with `/shard/{shard}` to match the nested peer router.
fn shard_topology(cluster: &ClusterConfig, shard: u32) -> Result<ShardTopology> {
    let Some(place) = cluster.replica else {
        return Ok(ShardTopology {
            node_id: 0,
            membership: Membership {
                voters: vec![0],
                learners: vec![],
            },
            peers: HashMap::new(),
            group: None,
        });
    };
    anyhow::ensure!(
        shard == place.shard_index,
        "pod replicates shard {} but was asked to host shard {shard}",
        place.shard_index
    );
    let replicas = place.replicas_per_shard.max(1);
    anyhow::ensure!(
        (1..=replicas).contains(&place.voter_count),
        "voter_count {} must be in 1..={replicas}",
        place.voter_count
    );
    let mut peers = HashMap::new();
    for r in 0..replicas {
        if r == place.replica_index {
            continue;
        }
        let base = cluster
            .peers
            .get(r as usize)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| anyhow::anyhow!("no peer URL for replica {r} of shard {shard}"))?;
        peers.insert(
            r as NodeId,
            format!("{}/shard/{shard}", base.trim_end_matches('/')),
        );
    }
    Ok(ShardTopology {
        node_id: place.replica_index as NodeId,
        membership: Membership {
            voters: (0..place.voter_count as NodeId).collect(),
            learners: (place.voter_count as NodeId..replicas as NodeId).collect(),
        },
        peers,
        group: Some(shard_group_id(shard)),
    })
}
