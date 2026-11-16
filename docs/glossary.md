# keep glossary

| Term | Meaning |
|------|---------|
| shard | A slice of the keyspace that one engine shard owns inside a node; `--shards` sets the count, which defaults to 256. |
| KvEngine | The sharded multi-core key-value engine in `keep-keyspace`. |
| WAL | The write-ahead log, which holds each mutation and is fsynced before the write is acknowledged. |
| snapshot | A saved copy of the engine state; `keep backup` recovers the latest snapshot and replays the WAL. |
| claim-check | A large payload kept as opaque bytes that travel as raw octet-stream, never through JSON. |
| raft host | The Raft instance that runs consensus for one owned shard; `keep-replication` runs one raft host per owned shard. |
| voter | A Raft member that takes part in votes; `voterCount` sets the voters per shard and defaults to 1. |
| learner | A Raft member that does not vote; keep sets the voters and learners at startup. |
| replica mode | Raft replica mode, which is on when the `raft` feature is built and `KEEP_REPLICAS_PER_SHARD` is above 1. |
| ratchet (perf gate) | A performance cell with `gate: ratchet` that passes when its ratio is at least the larger of `min_ratio` and the ratcheted baseline, and fails the nightly job on a regression. |
| gate (CI check) | The `gate` job in `.github/workflows/ci.yml`, the one required check; it passes only when every other job passes. |
| `aw` | The Faberline issue-flow CLI that claims an issue, runs a stage and opens the pull request. |
