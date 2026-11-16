# keep architecture

## Crates

| Crate | Path | Notes |
|-------|------|-------|
| `keep` | `crates/keep/` | Top-level facade library (`keep::*`), CLI/server binary (`src/bin/keep/main.rs`), integration tests, benchmarks, and examples. |
| `keep-shared-kernel` | `crates/keep-shared-kernel/` | Shared kernel: value types (`KvValue`, `SetOptions`, JSON conversion), error model (`KvError`), shard cluster topology, Prometheus metrics, and TLS helpers. |
| `keep-storage` | `crates/keep-storage/` | On-disk record format, write-ahead log, blob store, and persistence settings. No engine types. |
| `keep-keyspace` | `crates/keep-keyspace/` | Sharded multi-core KV engine (`KvEngine`) with its snapshot, recovery, and persistence handle (`persistence`). |
| `keep-replication` | `crates/keep-replication/` | Backup/restore and the optional `raft` per-shard hosts. |
| `keep-access` | `crates/keep-access/` | Bearer-token auth over the shared `service-auth` policy. |
| `keep-commands` | `crates/keep-commands/` | Axum over hyper HTTP/1.1 + HTTP/2 cleartext server, routes, handlers, request/response models, and OpenAPI spec generation (`/openapi.json`). |
| `keep-operator` | `crates/keep-operator/` | Kubernetes `KeepCluster` CRD, manifest renderer, and reconciliation loop. |
| `keep-client` | `crates/keep-client/` | In-tree async HTTP/2 client (`KvClient`) for scalar KV, batch, lock, and claim-check operations. |

The engine and persistence layers are transport-agnostic. Polyglot workers
integrate against the generated OpenAPI document. There is no language-specific
client to ship.

## Layers

Each context is split into layers. The layers are `domain`, `application`,
`infrastructure` and `interfaces`.

The rule for what each layer may use:

- `domain` uses nothing.
- `application` uses `domain`.
- `infrastructure` uses `domain`.
- `interfaces` uses `application`.

## Where state lives

- The on-disk record format, the write-ahead log (WAL) and the snapshots are in `keep-storage`.
- The sharded engine `KvEngine` is in `keep-keyspace`.
- The per-shard Raft hosts are in `keep-replication`.
