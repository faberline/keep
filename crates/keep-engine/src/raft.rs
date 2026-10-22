//! raft-runtime-backed consensus (HA phase C). keep's engine is wired as a
//! [`raft_runtime::RaftStateMachine`] so writes go through the shared driver
//! (propose → commit → apply); adopting [`RaftHost`] gives keep the proven h2c
//! peer transport + sole-applier read-your-write it previously lacked.
//!
//! - [`KvStateMachine`] is the engine fronting **one shard's** raft group. The
//!   log entry is a [`RaftCmd`]: a [`WalOp`] stamped with its proposal time
//!   (so relative TTLs become deadlines every replica agrees on), or a
//!   [`RaftCmd::Barrier`] a new leader commits to settle its term. Apply runs
//!   the op through [`crate::apply::execute`] and keeps the result in an
//!   [`OutcomeWindow`] keyed by log index, so the proposing handler answers
//!   from the replicated result instead of a racy local read.
//! - Snapshots carry each key's remaining TTL; restore clears the shard's
//!   keyspace before loading, so keys deleted after the snapshot cannot
//!   survive an InstallSnapshot.
//! - [`ShardHosts`] runs one [`RaftHost`] per hosted shard group; each host's
//!   peer router mounts under `/shard/{id}` so the Vote/Append/InstallSnapshot
//!   RPCs ride the serve port.
//!
//! In replica mode raft owns durability and memory admission: the engine runs
//! without its own WAL and with `maxmemory = 0` (no eviction — evicting on one
//! replica but not another would fork state); the configured limit is enforced
//! on the leader **before** an index is allocated ([`OOM_REJECTION`]).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::Router;
use raft_runtime::{
    AdmissionPermit, FsyncPolicy, GroupId, HostConfig, HostShutdownReport, Index, Membership,
    NodeId, OutcomeWindow, RaftHost, RaftStateMachine, RaftStore, SnapshotPolicy,
};
use serde::{Deserialize, Serialize};
use server_lifecycle::ShutdownDeadline;
use tokio::task::JoinHandle;

use crate::apply::{execute, ApplyOutcome, ApplyResult};
use crate::cluster::ClusterConfig;
use crate::engine::{EvictionPolicy, KvEngine};
use crate::error::KvError;
use crate::persistence::format::WalOp;
use crate::types::KvValue;

/// How many committed entries a shard's host applies before it captures a
/// snapshot and compacts the raft log (bounds the log; arms InstallSnapshot for
/// a lagging/fresh replica).
pub const SNAPSHOT_EVERY: u64 = 1024;

/// Reason prefix of a proposal the leader refused because the shard is at its
/// memory limit. Handlers map it to `507 out_of_memory`.
pub const OOM_REJECTION: &str = "keep: out of memory";

/// How often a host checks whether it just became leader (to commit a
/// [`RaftCmd::Barrier`]).
const LEADER_POLL: Duration = Duration::from_millis(50);

/// One raft log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RaftCmd {
    /// A write, stamped with the proposer's wall clock (ms since the epoch).
    /// Relative TTLs inside `op` are aged by the time between proposal and
    /// apply, so a replica that applies later (catch-up, restart replay) lands
    /// on the same deadline.
    Op { op: WalOp, at_ms: u64 },
    /// No-op a new leader commits so entries from earlier terms become
    /// committed (raft-core does not append one on election).
    Barrier,
}

impl RaftCmd {
    /// Decode a log entry. Entries written before `RaftCmd` existed are bare
    /// [`WalOp`]s; they replay with their TTLs unaged.
    pub fn decode(bytes: &[u8]) -> Result<(Option<WalOp>, Option<u64>)> {
        match serde_json::from_slice::<RaftCmd>(bytes) {
            Ok(RaftCmd::Op { op, at_ms }) => Ok((Some(op), Some(at_ms))),
            Ok(RaftCmd::Barrier) => Ok((None, None)),
            Err(_) => Ok((Some(serde_json::from_slice::<WalOp>(bytes)?), None)),
        }
    }
}

/// Wall clock in ms since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Shorten every relative TTL in `op` by `elapsed` (the time since the op was
/// proposed). A TTL that ran out before apply becomes zero (`EXPIRE`-style ops:
/// non-positive), which the engine treats as already expired.
pub fn age_ttls(op: WalOp, elapsed: Duration) -> WalOp {
    if elapsed.is_zero() {
        return op;
    }
    let age = |ttl: Option<Duration>| ttl.map(|d| d.saturating_sub(elapsed));
    let elapsed_ms = elapsed.as_millis().min(i64::MAX as u128) as i64;
    let age_ms = |ms: i64| {
        if ms <= 0 {
            ms
        } else {
            ms.saturating_sub(elapsed_ms).max(0)
        }
    };
    match op {
        WalOp::Set { key, value, ttl } => WalOp::Set {
            key,
            value,
            ttl: age(ttl),
        },
        WalOp::MSet { pairs, ttl } => WalOp::MSet {
            pairs,
            ttl: age(ttl),
        },
        WalOp::SetNx { key, value, ttl } => WalOp::SetNx {
            key,
            value,
            ttl: age(ttl),
        },
        WalOp::Cas {
            key,
            expected,
            new,
            ttl,
        } => WalOp::Cas {
            key,
            expected,
            new,
            ttl: age(ttl),
        },
        WalOp::Lock { key, owner, ttl } => WalOp::Lock {
            key,
            owner,
            ttl: ttl.saturating_sub(elapsed),
        },
        WalOp::ExtendLock { key, owner, ttl } => WalOp::ExtendLock {
            key,
            owner,
            ttl: ttl.saturating_sub(elapsed),
        },
        WalOp::Expire { key, seconds } => WalOp::PExpire {
            key,
            milliseconds: age_ms(seconds.saturating_mul(1000)),
        },
        WalOp::PExpire { key, milliseconds } => WalOp::PExpire {
            key,
            milliseconds: age_ms(milliseconds),
        },
        WalOp::GetEx {
            key,
            ttl_ms,
            persist,
        } => WalOp::GetEx {
            key,
            ttl_ms: ttl_ms.map(|ms| ms.saturating_sub(elapsed_ms as u64)),
            persist,
        },
        other => other,
    }
}

/// Whether `op` can grow resident memory (refused at the memory limit).
fn grows_memory(op: &WalOp) -> bool {
    !matches!(
        op,
        WalOp::Delete { .. }
            | WalOp::MDel { .. }
            | WalOp::Unlock { .. }
            | WalOp::Expire { .. }
            | WalOp::PExpire { .. }
            | WalOp::Persist { .. }
            | WalOp::HDel { .. }
            | WalOp::SRem { .. }
            | WalOp::ZRem { .. }
            | WalOp::LPop { .. }
            | WalOp::RPop { .. }
            | WalOp::GetEx { .. }
    )
}

/// One key in a shard snapshot: value plus remaining TTL in ms.
type SnapEntry = (String, KvValue, Option<u64>);

/// A shard's keyspace snapshot, tagged with the raft index it is current as of
/// (so `restore` can set the applied index, per the [`RaftStateMachine`]
/// contract) and the wall clock it was taken at (so a later restore ages the
/// TTLs). `values` is the pre-TTL format, still accepted on restore.
#[derive(Serialize, Deserialize)]
struct ShardSnapshot {
    up_to: Index,
    #[serde(default)]
    taken_at_ms: u64,
    #[serde(default)]
    entries: Vec<SnapEntry>,
    #[serde(default)]
    values: Vec<(String, KvValue)>,
}

/// keep's [`KvEngine`] driven as a [`RaftStateMachine`] for one shard's group.
///
/// The state machine is scoped to a `shard` so its snapshot/restore touch only
/// that shard's keys (apply only ever receives that shard's ops, because writes
/// are routed by key). `applied` is this shard group's own raft head.
pub struct KvStateMachine {
    engine: Arc<KvEngine>,
    cluster: ClusterConfig,
    shard: u32,
    applied: AtomicU64,
    outcomes: Mutex<OutcomeWindow<ApplyResult>>,
    /// Leader-side memory admission limit in bytes (0 = unlimited).
    memory_limit: usize,
}

impl KvStateMachine {
    /// Wrap `engine` as the state machine for `shard` of `cluster`, with no
    /// memory admission limit.
    pub fn new(engine: Arc<KvEngine>, cluster: ClusterConfig, shard: u32) -> Arc<Self> {
        Self::with_memory_limit(engine, cluster, shard, 0)
    }

    /// As [`new`](Self::new), refusing memory-growing proposals once the engine
    /// holds `memory_limit` bytes (0 = unlimited).
    pub fn with_memory_limit(
        engine: Arc<KvEngine>,
        cluster: ClusterConfig,
        shard: u32,
        memory_limit: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            cluster,
            shard,
            applied: AtomicU64::new(0),
            outcomes: Mutex::new(OutcomeWindow::default()),
            memory_limit,
        })
    }

    /// Take the outcome of the entry applied at `index` (each index is claimed
    /// once, by the handler that proposed it).
    pub fn claim(&self, index: Index) -> Option<ApplyResult> {
        self.outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .claim(index)
    }

    fn in_shard(&self, key: &str) -> bool {
        self.cluster.shard_for(key) == self.shard
    }
}

impl RaftStateMachine for KvStateMachine {
    fn admit_proposal(&self, command: &[u8]) -> Result<Option<AdmissionPermit>> {
        if self.memory_limit == 0 || self.engine.estimate_memory() < self.memory_limit {
            return Ok(None);
        }
        if let (Some(op), _) = RaftCmd::decode(command)? {
            if grows_memory(&op) {
                anyhow::bail!(
                    "{OOM_REJECTION}: shard {} holds >= {} bytes",
                    self.shard,
                    self.memory_limit
                );
            }
        }
        Ok(None)
    }

    fn apply(&self, index: Index, command: &[u8]) -> Result<()> {
        // A bad decode / engine error is a completed no-op whose error is the
        // entry's outcome; `applied` still advances (sole applier).
        let outcome = match RaftCmd::decode(command) {
            Ok((Some(op), at_ms)) => {
                let elapsed = at_ms
                    .map(|at| Duration::from_millis(now_ms().saturating_sub(at)))
                    .unwrap_or_default();
                execute(&self.engine, age_ttls(op, elapsed))
            }
            Ok((None, _)) => Ok(ApplyOutcome::Done),
            Err(e) => {
                tracing::warn!(index, error = %e, "raft: undecodable command (entry no-ops)");
                Err(KvError::Storage(format!("undecodable raft command: {e}")))
            }
        };
        {
            let mut window = self.outcomes.lock().unwrap_or_else(|e| e.into_inner());
            window.insert(index, outcome);
            window.advance(index);
        }
        self.applied.store(index, Ordering::Release);
        Ok(())
    }

    fn snapshot(&self, writer: &mut dyn std::io::Write) -> Result<()> {
        let entries = self
            .engine
            .dump_entries(|k| self.in_shard(k))
            .into_iter()
            .map(|(k, v, ttl)| (k, v, ttl.map(|d| d.as_millis() as u64)))
            .collect();
        let snap = ShardSnapshot {
            up_to: self.applied_index(),
            taken_at_ms: now_ms(),
            entries,
            values: Vec::new(),
        };
        serde_json::to_writer(writer, &snap)?;
        Ok(())
    }

    fn validate_snapshot(&self, reader: &mut dyn std::io::Read) -> Result<()> {
        serde_json::from_reader::<_, ShardSnapshot>(reader)?;
        Ok(())
    }

    fn restore(&self, reader: &mut dyn std::io::Read) -> Result<()> {
        let snap: ShardSnapshot = serde_json::from_reader(reader)?;
        // Replace, not merge: keys deleted after the snapshot must not survive.
        self.engine.remove_where(|k| self.in_shard(k));
        let elapsed = now_ms().saturating_sub(snap.taken_at_ms);
        for (k, v, ttl_ms) in snap.entries {
            let ttl = match ttl_ms {
                Some(ms) if ms <= elapsed => continue, // expired in flight
                Some(ms) => Some(Duration::from_millis(ms - elapsed)),
                None => None,
            };
            let _ = execute(
                &self.engine,
                WalOp::Set {
                    key: k,
                    value: v,
                    ttl,
                },
            );
        }
        self.engine.load_values(snap.values);
        self.applied.store(snap.up_to, Ordering::Release);
        Ok(())
    }

    fn applied_index(&self) -> Index {
        self.applied.load(Ordering::Acquire)
    }
}

/// A hosted shard group: its raft host and the state machine it drives.
struct ShardGroup {
    host: Arc<RaftHost>,
    sm: Arc<KvStateMachine>,
}

/// One [`RaftHost`] per hosted shard; a write routes to its key's shard host.
pub struct ShardHosts {
    cluster: ClusterConfig,
    groups: HashMap<u32, ShardGroup>,
    barrier_tasks: Vec<JoinHandle<()>>,
    pub engine: Arc<KvEngine>,
}

impl Drop for ShardHosts {
    fn drop(&mut self) {
        for task in &self.barrier_tasks {
            task.abort();
        }
    }
}

impl ShardHosts {
    /// Spawn the shard hosts this pod runs, persisting each group's hard state
    /// under `{data_dir}/raft/shard-{id}`.
    ///
    /// - Replica mode (`cluster.replica` set): **one** host, for this pod's own
    ///   shard group. Its raft id is the pod's replica index, voters are
    ///   `0..voter_count` and the rest learners (raft-runtime's `ClusterDims`
    ///   model); peers are the group's other replicas with a `/shard/{id}`
    ///   suffix for the shared h2c transport. The engine's `maxmemory` moves
    ///   into leader-side admission (see the module docs).
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
    /// (tests drive a small threshold so InstallSnapshot fires quickly).
    pub async fn with_snapshot_every(
        cluster: ClusterConfig,
        engine: Arc<KvEngine>,
        data_dir: &Path,
        snapshot_every: u64,
    ) -> Result<Self> {
        let memory_limit = if cluster.replica.is_some() {
            let limit = engine.get_maxmemory();
            engine.set_maxmemory(0);
            if limit > 0 && engine.get_eviction_policy() != EvictionPolicy::NoEviction {
                tracing::warn!(
                    "raft replica mode never evicts (replicas would diverge); \
                     maxmemory is enforced as noeviction"
                );
            }
            limit
        } else {
            0
        };
        let mut groups = HashMap::new();
        let mut barrier_tasks = Vec::new();
        for shard in cluster.owned_shards() {
            let topo = shard_topology(&cluster, shard)?;
            let dir = data_dir.join("raft").join(format!("shard-{shard}"));
            std::fs::create_dir_all(&dir)?;
            let dir = dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("raft data dir is not valid UTF-8"))?;
            let sm = KvStateMachine::with_memory_limit(
                engine.clone(),
                cluster.clone(),
                shard,
                memory_limit,
            );
            let cfg = HostConfig {
                snapshot: SnapshotPolicy::EveryEntries(snapshot_every),
                ..Default::default()
            };
            let dyn_sm = sm.clone() as Arc<dyn RaftStateMachine>;
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
                        dyn_sm,
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
                        dyn_sm,
                        cfg,
                    )
                }
            };
            let host = Arc::new(host);
            barrier_tasks.push(tokio::spawn(barrier_on_election(host.clone(), shard)));
            groups.insert(shard, ShardGroup { host, sm });
        }
        Ok(Self {
            cluster,
            groups,
            barrier_tasks,
            engine,
        })
    }

    /// Graceful stop for SIGTERM: every host stops admitting proposals, a
    /// leader hands leadership to a caught-up voter, then its background tasks
    /// and in-flight peer RPCs drain. All hosts share one `budget`; a host that
    /// runs out of it reports the phase it stopped in.
    pub async fn shutdown_within(&self, budget: Duration) -> Vec<(u32, HostShutdownReport)> {
        for task in &self.barrier_tasks {
            task.abort();
        }
        let deadline = ShutdownDeadline::from_now(budget, Duration::ZERO)
            .expect("a zero reserve never exceeds the budget");
        let mut reports = Vec::with_capacity(self.groups.len());
        for (shard, group) in &self.groups {
            reports.push((*shard, group.host.shutdown_within(deadline).await));
        }
        reports
    }

    /// Route `op` to the host owning `key`'s shard and propose it through Raft.
    /// Resolves once **this node's** engine has applied the entry, with the
    /// result the state machine computed for it (read-your-write). Errors if
    /// `key`'s shard is not hosted here or the proposal did not commit.
    pub async fn write(&self, key: &str, op: WalOp) -> Result<ApplyResult> {
        let group = self
            .group_for(key)
            .ok_or_else(|| anyhow::anyhow!("shard for key '{key}' not hosted by this node"))?;
        let cmd = RaftCmd::Op {
            op,
            at_ms: now_ms(),
        };
        let index = group.host.propose(serde_json::to_vec(&cmd)?).await?;
        group
            .sm
            .claim(index)
            .ok_or_else(|| anyhow::anyhow!("raft: outcome of entry {index} was not retained"))
    }

    /// Commit a [`RaftCmd::Barrier`] in `key`'s shard group; resolves with its
    /// index once applied locally.
    pub async fn barrier(&self, key: &str) -> Result<Index> {
        let group = self
            .group_for(key)
            .ok_or_else(|| anyhow::anyhow!("shard for key '{key}' not hosted by this node"))?;
        let index = group.host.propose(barrier_command()).await?;
        group.sm.claim(index);
        Ok(index)
    }

    /// Merge every shard host's peer router, each nested under `/shard/{id}`, so
    /// the Vote/Append/InstallSnapshot + leader-redirect RPCs ride the service's
    /// h2c serve port. Peer base URLs carry the matching `/shard/{id}` prefix.
    pub fn router(&self) -> Router {
        let mut app = Router::new();
        for (shard, group) in &self.groups {
            app = app.merge(Router::new().nest(&format!("/shard/{shard}"), group.host.router()));
        }
        app
    }

    /// Shard a key belongs to.
    pub fn shard_for(&self, key: &str) -> u32 {
        self.cluster.shard_for(key)
    }

    /// Whether this node hosts `key`'s shard group.
    pub fn owns(&self, key: &str) -> bool {
        self.group_for(key).is_some()
    }

    /// Number of per-shard hosts this node runs.
    pub fn host_count(&self) -> usize {
        self.groups.len()
    }

    /// The host driving `key`'s shard group (for status / direct propose).
    pub fn host_for(&self, key: &str) -> Option<&Arc<RaftHost>> {
        self.group_for(key).map(|g| &g.host)
    }

    fn group_for(&self, key: &str) -> Option<&ShardGroup> {
        self.groups.get(&self.cluster.shard_for(key))
    }
}

fn barrier_command() -> Vec<u8> {
    serde_json::to_vec(&RaftCmd::Barrier).expect("barrier encodes")
}

/// Commit a barrier each time `host` becomes leader. raft-core does not append
/// a no-op on election, so without it entries from earlier terms stay
/// uncommitted until the next client write.
async fn barrier_on_election(host: Arc<RaftHost>, shard: u32) {
    let mut was_leader = false;
    let mut tick = tokio::time::interval(LEADER_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let leader = host.is_leader().await;
        if leader && !was_leader {
            if let Err(e) = host.propose(barrier_command()).await {
                tracing::warn!(shard, error = %e, "raft: leader barrier failed; retrying");
                continue;
            }
            tracing::info!(shard, "raft: leader barrier committed");
        }
        was_leader = leader;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::KvKey;

    fn cmd(op: WalOp, at_ms: u64) -> Vec<u8> {
        serde_json::to_vec(&RaftCmd::Op { op, at_ms }).unwrap()
    }

    fn set(key: &str, v: i64, ttl: Option<Duration>) -> WalOp {
        WalOp::Set {
            key: key.into(),
            value: KvValue::Int(v),
            ttl,
        }
    }

    fn sm() -> (Arc<KvEngine>, Arc<KvStateMachine>) {
        let engine = Arc::new(KvEngine::new());
        let sm = KvStateMachine::new(engine.clone(), ClusterConfig::default(), 0);
        (engine, sm)
    }

    fn key(k: &str) -> KvKey {
        KvKey::new(k).unwrap()
    }

    #[test]
    fn legacy_bare_walop_entries_still_decode() {
        let legacy = serde_json::to_vec(&set("a", 1, None)).unwrap();
        let (op, at) = RaftCmd::decode(&legacy).unwrap();
        assert!(matches!(op, Some(WalOp::Set { .. })));
        assert_eq!(at, None);
        let (op, _) = RaftCmd::decode(&barrier_command()).unwrap();
        assert!(op.is_none());
    }

    #[test]
    fn apply_records_outcomes_by_index() {
        let (_, sm) = sm();
        sm.apply(1, &cmd(set("a", 1, None), now_ms())).unwrap();
        let incr = WalOp::Incr {
            key: "a".into(),
            delta: 2,
        };
        sm.apply(2, &cmd(incr, now_ms())).unwrap();
        sm.apply(3, &barrier_command()).unwrap();
        assert_eq!(sm.applied_index(), 3);
        assert_eq!(sm.claim(2).unwrap().unwrap(), ApplyOutcome::Int(3));
        assert!(sm.claim(2).is_none(), "an outcome is claimed once");
        assert_eq!(sm.claim(3).unwrap().unwrap(), ApplyOutcome::Done);
        // A refused op is a completed no-op carrying the engine error.
        let string = WalOp::Set {
            key: "s".into(),
            value: KvValue::String("x".into()),
            ttl: None,
        };
        sm.apply(4, &cmd(string, now_ms())).unwrap();
        let bad = WalOp::Incr {
            key: "s".into(),
            delta: 1,
        };
        sm.apply(5, &cmd(bad, now_ms())).unwrap();
        assert!(matches!(
            sm.claim(5).unwrap(),
            Err(KvError::TypeMismatch { .. })
        ));
        assert_eq!(sm.applied_index(), 5);
    }

    #[test]
    fn ttls_age_from_proposal_time() {
        let (engine, sm) = sm();
        let proposed = now_ms() - 60_000;
        // Proposed a minute ago with a 30s TTL: already expired on apply.
        sm.apply(
            1,
            &cmd(set("gone", 1, Some(Duration::from_secs(30))), proposed),
        )
        .unwrap();
        assert!(!engine.exists(&key("gone")));
        // 2 min TTL proposed a minute ago: about one minute left.
        sm.apply(
            2,
            &cmd(set("kept", 1, Some(Duration::from_secs(120))), proposed),
        )
        .unwrap();
        let left = engine.pttl(&key("kept"));
        assert!((55_000..=60_000).contains(&left), "pttl {left}");
        // EXPIRE is converted to an aged PEXPIRE.
        let expire = WalOp::Expire {
            key: "kept".into(),
            seconds: 90,
        };
        sm.apply(3, &cmd(expire, proposed)).unwrap();
        let left = engine.pttl(&key("kept"));
        assert!((25_000..=30_000).contains(&left), "pttl {left}");
    }

    #[test]
    fn snapshot_restore_replaces_keyspace_and_keeps_ttls() {
        let (_, leader) = sm();
        leader.apply(1, &cmd(set("a", 1, None), now_ms())).unwrap();
        leader
            .apply(
                2,
                &cmd(set("t", 2, Some(Duration::from_secs(60))), now_ms()),
            )
            .unwrap();
        let mut bytes = Vec::new();
        leader.snapshot(&mut bytes).unwrap();
        leader.validate_snapshot(&mut bytes.as_slice()).unwrap();

        let (engine, follower) = sm();
        follower
            .apply(1, &cmd(set("stale", 9, None), now_ms()))
            .unwrap();
        follower.restore(&mut bytes.as_slice()).unwrap();
        assert_eq!(follower.applied_index(), 2);
        assert!(
            !engine.exists(&key("stale")),
            "restore must clear the shard"
        );
        assert_eq!(engine.get(&key("a")), Some(KvValue::Int(1)));
        let left = engine.pttl(&key("t"));
        assert!((55_000..=60_000).contains(&left), "pttl {left}");
    }

    #[test]
    fn restore_only_clears_its_own_shard() {
        let cluster = ClusterConfig::new(0, 1, 4, vec![]);
        let engine = Arc::new(KvEngine::new());
        let other = (0..100)
            .map(|i| format!("k{i}"))
            .find(|k| cluster.shard_for(k) != 0)
            .unwrap();
        engine.set(&key(&other), KvValue::Int(7), None).unwrap();
        let sm = KvStateMachine::new(engine.clone(), cluster, 0);
        let empty = serde_json::to_vec(&ShardSnapshot {
            up_to: 5,
            taken_at_ms: now_ms(),
            entries: vec![],
            values: vec![],
        })
        .unwrap();
        sm.restore(&mut empty.as_slice()).unwrap();
        assert_eq!(engine.get(&key(&other)), Some(KvValue::Int(7)));
    }

    #[test]
    fn admission_refuses_growth_at_the_memory_limit() {
        let engine = Arc::new(KvEngine::new());
        engine.set(&key("big"), KvValue::Int(1), None).unwrap();
        let sm = KvStateMachine::with_memory_limit(engine, ClusterConfig::default(), 0, 1);
        let err = sm
            .admit_proposal(&cmd(set("x", 1, None), now_ms()))
            .unwrap_err();
        assert!(err.to_string().starts_with(OOM_REJECTION));
        let del = WalOp::Delete { key: "big".into() };
        assert!(sm.admit_proposal(&cmd(del, now_ms())).is_ok());
        assert!(sm.admit_proposal(&barrier_command()).is_ok());
    }
}
