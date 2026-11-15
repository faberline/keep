# Keep configuration reference

This page lists every flag and environment variable of the `keep` binary and
every field of the `Keep` custom resource. The flags and their defaults come
from `keep --help`; the tests in `crates/keep/tests/docs_lint.rs` check that
every long flag in that help appears here.

Each server flag has an environment variable. A flag given on the command line
wins over its variable. The server flags belong to the bare `keep` command:
there is no `serve` subcommand.

## Server flags and environment variables

| Flag | Env var | Default | Meaning |
|---|---|---|---|
| `--host` | `KEEP_HOST` | `127.0.0.1` | Bind host. Kubernetes passes `0.0.0.0`. |
| `--port` | `KEEP_PORT` | `7117` | HTTP port. HTTP/1.1 and HTTP/2 cleartext share it. |
| `--worker-threads` | `KEEP_WORKER_THREADS` | `0` | Tokio worker threads. `0` detects the CPU count or cgroup quota. |
| `--shards` | `KEEP_SHARDS` | `256` | Engine shards inside one node, for multi-core scaling. `0` derives it from the worker threads. |
| `--maxmemory` | `KEEP_MAXMEMORY` | unset | Memory cap for engine entries, such as `512Mi`, `4Gi` or bytes. `0` is unlimited. When unset, `--maxmemory-ratio` of the cgroup limit applies. |
| `--maxmemory-ratio` | `KEEP_MAXMEMORY_RATIO` | `0.8` | Share of the cgroup memory limit to use when `--maxmemory` is unset. |
| `--eviction-policy` | `KEEP_EVICTION_POLICY` | `allkeys-lru` | Memory eviction policy: `noeviction`, `allkeys-lru`, `volatile-lru`, `allkeys-lfu` or `allkeys-random`. |
| `--spill-threshold-bytes` | `KEEP_SPILL_THRESHOLD_BYTES` | `65536` | Size above which `Bytes` values spill to `/data/blobs/` when persistence is on. `0` turns disk blob tiering off. |
| `--max-disk-bytes` | `KEEP_MAX_DISK_BYTES` | unset | Disk cap for tiered blobs, such as `10Gi` or bytes. Unset or `0` is unlimited. |
| `--expiry-interval-ms` | `KEEP_EXPIRY_INTERVAL_MS` | `1000` | Interval of the active TTL expiry sweep in milliseconds. `0` turns it off. |
| `--api-token` | `KEEP_API_TOKEN` | unset | Bearer token that every data-plane route requires. |
| `--api-token-readonly` | `KEEP_API_TOKEN_READONLY` | unset | Bearer token that may only read. A write with it gets `403`. |
| `--tls-cert` | `KEEP_TLS_CERT` | unset | PEM certificate chain. With `--tls-key`, the public API is served over HTTPS. |
| `--tls-key` | `KEEP_TLS_KEY` | unset | PEM private key for `--tls-cert`. |
| `--log-level` | `KEEP_LOG_LEVEL` | `info` | `trace`, `debug`, `info`, `warn` or `error`. `RUST_LOG` overrides it. |
| `--log-format` | `KEEP_LOG_FORMAT` | `pretty` | Stdout format: `pretty` for local use, or `json` for a log collector. |
| `--otlp-endpoint` | `KEEP_OTLP_ENDPOINT` | unset | OTLP endpoint for trace export. |
| `--data-dir` | `KEEP_DATA_DIR` | `./data` | Directory for the WAL, snapshots and tiered blobs. |
| `--disable-persistence` | `KEEP_DISABLE_PERSISTENCE` | `false` | Run in memory only, with no durability. |
| `--fsync-interval-ms` | `KEEP_FSYNC_MS` | `100` | WAL fsync interval in milliseconds. |
| `--snapshot-interval-secs` | `KEEP_SNAPSHOT_SECS` | `300` | Snapshot interval in seconds. |
| `--snapshot-ops-threshold` | `KEEP_SNAPSHOT_OPS` | `100000` | Number of operations that triggers a snapshot. |
| `--body-limit` | `KEEP_BODY_LIMIT` | `16777216` (16 MiB) | Largest request body in bytes. It bounds claim-check blob writes. |
| `--grace-secs` | `KEEP_GRACE_SECS` | `30` | Drain window in seconds after SIGTERM. |
| `--node-id` | `KEEP_NODE_ID` | `0` | This node's ordinal in the cluster. Kubernetes uses the StatefulSet pod index. |
| `--node-count` | `KEEP_NODE_COUNT` | `1` | Number of nodes in the cluster. |
| `--shard-count` | `KEEP_SHARD_COUNT` | `1` | Virtual shard count for client-side routing. It must be at least `--node-count`. |
| `--peers` | `KEEP_PEERS` | unset | Peer base URLs, comma-separated. The index is the node ordinal. |

## Environment variables without a flag

| Env var | Read in | Meaning |
|---|---|---|
| `KEEP_TOKEN_SECRET` | `crates/keep/src/bin/keep/serve.rs` | Turns on scoped claim-check tokens for the `/inputs/*` and `/results/*` routes. Unset or empty means no enforcement. |
| `KEEP_REPLICAS_PER_SHARD` | `crates/keep/src/bin/keep/serve.rs` | Replica count per shard when raft replica mode is on. It is read only in a build with the `raft` feature, and defaults to `1`. |
| `KEEP_HEADLESS_SERVICE` | `crates/keep/src/bin/keep/serve_config.rs` | Headless Kubernetes service name used to find raft peers. Defaults to `keep-headless`. |

Test and perf-gate variables, such as `KEEP_PERF_*` and `KEEP_UPDATE_SURFACE`,
are not listed here.

## Subcommands

- `llm`: prints the agent-facing topics offline. `--format json` gives a machine-readable form.
- `upgrade`: replaces the binary with a published release. `--check` reports the available version without changing anything.
- `issue`: searches, views, files and comments on keep issues on the tracker.
- `k8s`: renders the CRD, the operator and the app-namespace Keep instances. Only `operator run` needs the `operator` build feature.
- `backup`: writes a snapshot of the on-disk state to `--dest`, a `file://` destination.
- `restore`: loads the newest backup into `--data-dir`. It refuses a non-empty directory unless `--force` is given.
- `spec`: prints the OpenAPI document offline. `--format` selects the output, `--shapes` prints the request cookbook and `--fields` prints the value-type catalog. `spec gen --lang` with `--out` writes a typed client.
- `dockerfile`: renders the runtime image Dockerfiles offline.

## Keep resource (spec)

The operator reads the `spec` of a `Keep` object (`keep.dev/v1alpha1`). Defaults
come from `k8s/operator/crd.yaml`.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `image` | string | required | Container image of the keep server. |
| `imagePullPolicy` | string | unset | Image pull policy. |
| `shardCount` | integer | `1` | Virtual shards for client-side routing. |
| `replicasPerShard` | integer | `1` | Starting and minimum members per shard. The count is fixed at startup. |
| `voterCount` | integer | `1` | Raft voters per shard. |
| `resources.cpu` | string | empty | CPU request. An empty value uses the data-plane default of 1 CPU. |
| `resources.memory` | string | empty | Memory request. An empty value uses the data-plane default of 4Gi. |
| `engineShards` | integer | `256` | Engine shards inside one node (`--shards`). |
| `logLevel` | string | `info` | Log level (`--log-level`). |
| `graceSecs` | integer | `30` | Drain window after SIGTERM (`--grace-secs`). |
| `storage` | string | `10Gi` | Size of the durable disk tier, the PVC for the WAL and snapshots. |
| `storageClass` | string | cluster default | StorageClass of that PVC. |
| `backup` | object | unset | Scheduled backup: `schedule` (cron), `destination` (URI) and optional `retentionSecs`. |
| `bodyLimitBytes` | integer | `16777216` | Request body limit (`--body-limit`). |
| `maxmemory` | string | auto | Memory cap (`--maxmemory`). When unset, the cgroup limit times `--maxmemory-ratio` applies. |
| `evictionPolicy` | string | `allkeys-lru` | Eviction policy (`--eviction-policy`). |
| `spillThresholdBytes` | integer | `65536` | Spill threshold (`--spill-threshold-bytes`). |
| `maxDiskBytes` | string | unlimited | Disk cap for tiered blobs (`--max-disk-bytes`). |

## Keep resource (status)

The operator writes these fields back to the `status` of a `Keep` object.

| Field | Meaning |
|---|---|
| `phase` | `Pending`, `Reconciling` or `Ready`. |
| `observedGeneration` | The object generation that this status reflects. |
| `readyReplicas` | Ready store replicas, from the StatefulSet status. |
| `desiredReplicas` | Desired store replicas: `shardCount` times `replicasPerShard`. |
| `shardCount` | Effective cluster shard count. |
| `message` | Last reconcile message. |
