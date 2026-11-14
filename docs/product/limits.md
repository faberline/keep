# Keep limits

This page lists the limits that the code sets. Each number names its constant
or flag. Where the code sets no limit, the page says `not limited by keep`.
The defaults are the ones in `keep --help` (see `configuration.md`).

## Sizes

- Key: at most 256 bytes. `KvKey::new` in `crates/keep-shared-kernel/src/types.rs`
  rejects a longer key with `KeyTooLong`, and an empty key with `EmptyKey`.
- Request body: 16 MiB by default (`DEFAULT_BODY_LIMIT` in
  `crates/keep-commands/src/http.rs`). Set it with `--body-limit` or
  `KEEP_BODY_LIMIT`; the custom resource field is `bodyLimitBytes`.
- Value: keep sets no separate value-size cap. Over HTTP, the body limit is the
  bound for a value. The body limit also bounds claim-check blob writes.
- Spill: a `Bytes` value larger than 65536 bytes goes to `/data/blobs/` when
  persistence is on (`--spill-threshold-bytes`). `0` turns this off.
- Memory: `--maxmemory` caps the engine entries. When it is unset, the cgroup
  memory limit times `--maxmemory-ratio` (0.8) applies.
- Disk: `--max-disk-bytes` caps the tiered blobs. Unset or `0` is not limited.

## Rates

- keep does not limit request rates. Limit them in front of keep.
- Long-poll wait: a request may wait at most 60 000 ms (`MAX_TIMEOUT_MS` in
  `crates/keep-commands/src/http/lists.rs`).
- Expiry sweep: every 1000 ms by default (`--expiry-interval-ms`).
- WAL fsync: every 100 ms by default (`--fsync-interval-ms`).
- Snapshot: every 300 seconds (`--snapshot-interval-secs`) or every 100 000
  operations (`--snapshot-ops-threshold`), whichever comes first.

## Shards and replicas

- Engine shards per node: 256 by default (`--shards`, custom resource
  `engineShards`).
- Cluster shards: 1 by default (`--shard-count`, custom resource `shardCount`).
  `--shard-count` must be at least `--node-count`.
- Replicas per shard: 1 by default (custom resource `replicasPerShard`). The
  raft membership is fixed at startup, so the replica count does not change
  while the cluster runs.
- Voters per shard: 1 by default (custom resource `voterCount`).

## Known limits

- The raft membership is static: the voters and learners are set at startup,
  and keep has no membership admin API yet. Change the replica count with a
  restart.
- Tokens are read once at startup, with no hot reload. Rotate them as the
  runbook says (`docs/product/runbook.md`, section `Token rotation`).
- Scoped claim-check tokens (`KEEP_TOKEN_SECRET`) cover only the `/inputs/*`
  and `/results/*` routes.
- The body limit (`--body-limit`) is the only size bound on a value sent over
  HTTP.
- keep does not limit request rates. See `## Rates`.
