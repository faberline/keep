---
id: keep-ha-raft-sharding-roadmap
summary: Keep HA sharding and raft rollout design, preserving the phase plan behind capability keep:ha-raft and keep:primary-replicas.
fill_sections: [logic, e2e-test]
---

# TD: Keep HA / Raft Sharding Roadmap (#121)

HA lands in phases. **Phase A is implemented.** Phase C (raft per shard group)
is implemented behind the `raft` feature (on by default): replicated writes,
the failover gate, and read consistency. Phase B is delivered as raft learners
inside the Phase C groups. Membership changes on scale events remain. The
phases share one substrate: node identity + the keyspace split
(`keep-shared-kernel/src/cluster.rs`).

## Phase A — sharded scale-out (DONE)

`src/cluster.rs` + `GET /cluster`. The keyspace is `shard_count` virtual shards
mapped onto `node_count` nodes (`keep-0..keep-N`, a StatefulSet). A client routes:

```
shard = crc32(key) % shard_count
node  = shard % node_count          # owner_of_shard
```

Each node owns a disjoint shard subset (proven in `cluster::tests`); a node is
independent (its own WAL + durability). `GET /cluster` reports
`{node_id, node_count, shard_count, owned_shards, peers, mode}` so a client (or a
thin proxy) can self-route — keep ships no client, so routing is a documented
contract, same as the connection-count guidance.

Config: `KEEP_NODE_ID` (pod ordinal), `KEEP_NODE_COUNT`, `KEEP_SHARD_COUNT`,
`KEEP_PEERS`. Single-node default (`mode=single`, one node owns everything).

**Gives:** capacity + throughput scale-out, blast-radius isolation.
**Does not give:** redundancy — a node loss makes its shards unavailable.

## Phase B — async replicas as raft learners (DONE)

The original sketch streamed a primary's WAL to read replicas. That design
loses the unreplicated tail on primary loss and would be a second replication
protocol next to raft. Phase B instead runs read replicas as **raft learners**
of the shard group: `VOTER_COUNT < REPLICAS_PER_SHARD` makes replicas
`VOTER_COUNT..REPLICAS_PER_SHARD` learners.

- A learner receives every committed entry (and InstallSnapshot when behind),
  so it loses nothing a voter would have; it never counts toward quorum, so a
  slow or stopped learner does not slow writes.
- A learner never becomes leader; failover is among voters only.
- Learners serve `bounded(<ms>)` and `any` reads locally, and `leader` reads
  through a forwarded barrier (see read consistency below). Writes sent to a
  learner are forwarded to the leader.
- `/cluster` reports a learner's `role` as `learner`.

Proven by `raft_cluster_process.rs::learner_replicates_without_joining_quorum`.

## Phase C — raft / quorum via `raft-runtime` (per-shard groups, DONE)

keep uses the shared **`raft-runtime`** `RaftHost` over `raft-core`, with h2c
peer transport on the service port, snapshots and compaction.

### Topology

raft-runtime's `ClusterDims` model: the StatefulSet has
`SHARD_COUNT × REPLICAS_PER_SHARD` pods; pod `keep-<i>` hosts exactly one shard
group (`shard = i % SHARD_COUNT`) as replica `i / SHARD_COUNT`, which is its
raft id. Voters are `0..VOTER_COUNT`, learners the rest. Without
replica placement (single node) the node runs one sole-voter group per owned
shard. `KEEP_REPLICAS_PER_SHARD` is accepted as a deprecated alias.

Replica mode keeps all state in raft (its own log, hard state and snapshots
under the data dir); the engine's own WAL recovery is skipped so no entry is
applied twice on restart.

### State machine

- **Command = `RaftCmd`**: `Op(WalOp)` or `Barrier`, tagged JSON; a bare
  legacy `WalOp` payload still decodes as `Op`. Relative TTLs are rewritten to
  absolute deadlines before proposing so every replica expires the same key at
  the same instant.
- **`KvStateMachine`** applies an `Op` through the WAL-replay apply path and
  records its `ApplyOutcome` per log index (`OutcomeWindow`), so the HTTP
  response is the replicated outcome rather than a racing local read.
- **Snapshot** = the shard's keys with absolute expiry; restore clears the shard
  first so deleted keys do not survive.
- A new leader proposes a `Barrier` so entries from earlier terms commit.
- Batches that span shards are rejected with `400 cross_shard_batch`.

### Failover

SIGTERM drains HTTP, then `ShardHosts::shutdown_within(KEEP_RAFT_HANDOFF_SECS)`
transfers leadership to a caught-up voter. `KEEP_RAFT_SNAPSHOT_EVERY` sets the
compaction interval. `terminationGracePeriodSeconds` must cover the HTTP grace
plus the handoff budget.

The gate is `crates/keep/tests/raft_cluster_process.rs`: three real `keep`
processes, real signals.

| case | proves |
|---|---|
| `sigkilled_leader_keeps_committed_writes` | acknowledged writes survive leader crash; rejoin applies nothing twice |
| `wiped_follower_catches_up_via_install_snapshot` | a replica with an empty disk catches up from a snapshot |
| `restarted_follower_snapshot_drops_deleted_keys` | snapshot restore replaces, not merges |
| `stopped_leader_is_deposed_and_converges` | a hung leader is replaced and never serves a stale default read after resuming |
| `reads_fail_closed_without_quorum` | without quorum a default read is 503, `any` still answers |
| `learner_replicates_without_joining_quorum` | Phase B learners (above) |
| `sigterm_leader_hands_off_leadership` | rolling restart transfers leadership |
| `uncommitted_tail_is_truncated_on_rejoin` (nightly, `--ignored`) | unacknowledged entries never become visible |

### Read consistency

Every data-plane read on a replicated group honors raft-runtime's
`x-read-consistency` header (`keep-commands/src/http/consistency.rs`,
`keep-replication/src/raft_read.rs`):

| level | served when | else |
|---|---|---|
| `leader` (default, also for unknown values) | a barrier that started after the request is applied on this replica | 503 `read_fence_failed` (3s) |
| `bounded(<ms>)` | this replica's proven staleness ≤ ms; a leader over the bound fences first | 503 `replica_too_stale` |
| `any` | always | — |

- `leader` reads are linearizable on **any** replica: a follower's or learner's
  barrier is forwarded to the leader, and the replica answers once it has
  applied the barrier. A deposed leader cannot commit its barrier, so it cannot
  answer. Concurrent reads share one barrier.
- Staleness is the time since this replica last provably held every committed
  entry: a completed barrier, or (on non-leaders) a sample of the leader's
  `/raftz` commit index, polled every 100ms, once applied locally.
- `bounded` and `any` responses carry `x-keep-staleness-ms` when known.
- Reads are served instead of redirected (no `421`): forwarding the barrier
  gives the same guarantee without a client round trip.
- `GET /cluster` adds `shards[]`: role, term, leader, commit/applied/snapshot
  index, voters, learners and `staleness_ms` per hosted group.

### Known limits

- A non-leader's `leader` read costs about one heartbeat (~60–100ms): raft-core
  tells followers the commit index only on the next heartbeat.
- No leader lease: in a minority partition that can still reach the old leader,
  a follower's `bounded` staleness may be optimistic. `leader` reads stay safe.
- Lazy TTL expiry is evaluated against each replica's clock, so clock skew
  shifts when a key disappears on different replicas.
- Reads do not check that the key belongs to the node's shard; clients route
  by `/cluster`.
- `/info` reports `maxmemory=0` in replica mode.

### Remaining staged work

- **Membership changes** — add/remove replicas on scale events
  (`upsert_peer → add_learner → catch up → promote`, and the reverse with a
  leadership transfer), an admin endpoint, and operator reconcile ordering.
- **raft-runtime follow-ups** (faberline/core): a public `RaftHost::status()`
  (keep reads `/raftz` in-process today), commit of a no-op on
  `become_leader` (keep proposes `Barrier`), `read_index()` or a leader lease
  (keep fences every read batch), and learner lag in status (keep polls the
  leader).

### Original design notes (historical openraft 0.9 sketch)

1. **TypeConfig** (`declare_raft_types!`): node id = pod ordinal; request =
   `Command` (an enum mirroring `WalOp` — the existing logical mutations);
   response = the op result.
2. **State machine** (`RaftStateMachine`): apply a committed `Command` to
   `KvEngine` (the apply path already exists — it's `recovery::apply_wal_operation`,
   which maps every `WalOp` onto an engine call; reuse it verbatim). Snapshot =
   the existing `SnapshotWriter`/engine `export_*`.
3. **Log storage** (`RaftLogStorage`): **the raft log SUBSUMES the current WAL.**
   keep already has a segmented, CRC'd, fsync'd append log (`persistence::wal`)
   with group commit — wrap it as the raft log store (vote + committed index +
   entries). This is the invasive part: the write path moves from
   "engine.set → log_wal → apply" to "raft.client_write(Command) → quorum commit
   → state-machine apply". Durable-before-ack becomes "replicated-and-fsynced
   before ack" — strictly stronger; the group-commit + `durability_barrier`
   machinery maps onto raft's apply notification.
4. **Network** (`RaftNetworkFactory`/`RaftNetwork`): AppendEntries / Vote /
   InstallSnapshot as HTTP/2 POSTs to `peers[node]` (reuse the existing hyper
   client stack; peers come from `ClusterConfig`). A small `/raft/*` internal
   route group, auth-gated off the public API.
5. **Membership / discovery**: k8s StatefulSet ordinals → stable DNS
   (`keep-<i>.keep-headless`); `ClusterConfig::peers` already models this. Initial
   cluster = `initialize()` with the ordinal set; changes via `change_membership`.
6. **Reads**: leader reads by default; bounded-lag follower reads via a
   `x-read-consistency` header (mirror lumen's `ReadConsistency`).

These notes predate the move to `raft-runtime`; the sections above describe
what shipped.
