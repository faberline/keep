# Keep data-plane runbook

This runbook covers the data plane of one Keep server: health and metrics, the
log format, and the procedures below. The `keep` commands here are checked against
the real CLI help by `crates/keep/tests/it/docs_lint.rs`. `curl` and `kubectl`
lines in this page are not tested.

## Health and metrics

| Endpoint | Response | Meaning |
|---|---|---|
| `GET /healthz` | `200 ok` | Liveness: the process answers. |
| `GET /readyz` | `200 ok`, or `503 draining` | Readiness. `503 draining` starts at SIGTERM and lasts for the drain window `--grace-secs` (env `KEEP_GRACE_SECS`, default `30`). |
| `GET /metrics` | Prometheus text | Metrics, listed below. |
| `GET /info` | JSON | Server state, listed below. |

`GET /info` returns these fields: `keys`, `shards`, `memory_bytes`,
`maxmemory_bytes`, `disk_blob_bytes`, `evicted_keys`, `spilled_keys`,
`eviction_policy`, `persistence_enabled`, `fsync_interval_ms` (`null` when
persistence is off), `version`.

`GET /metrics` exports these series:

- `keep_http_requests_total`
- `keep_http_request_duration_seconds`
- `keep_keys_total`
- `keep_memory_bytes`
- `keep_maxmemory_bytes`
- `keep_disk_blob_bytes`
- `keep_evicted_keys_total`
- `keep_spilled_keys_total`

## Log fields

Start the server with `--log-format json` (env `KEEP_LOG_FORMAT`, default
`pretty`). Each stdout line is one JSON object:

```bash
keep --disable-persistence --log-format json
```

Each line has these top-level fields: `schema` (always `axiom.service.log.v1`),
`timestamp`, `severity`, `service` (with `service.name` = `keep` and
`service.version`), `event`, `message`, and `attributes`.

When a request carries a valid W3C `traceparent` header, the request log line
also has `trace_id` and `span_id`, plus `trace_flags`. It has `parent_span_id`
only when the header is valid. A request log line carries the request path in
`attributes.uri`.

`RUST_LOG` overrides `--log-level` (env `KEEP_LOG_LEVEL`, default `info`).

## Node down

1. Find the pod: `kubectl get pods -l app=keep`.
2. Read its state and last logs: `kubectl describe pod <pod>` and
   `kubectl logs <pod>`.
3. Check `GET /readyz` on the pod.

The StatefulSet `keep` restarts the pod, and the pod keeps its volume claim. At
startup the server logs `recovering engine` (field `data_dir`), then
`recovery complete` with the fields `snapshot_entries`, `wal_replayed` and
`corrupted`. Those fields show how much state came back from the snapshot and
the write-ahead log.

kubectl lines are not tested.

## Disk full

1. Check how much disk the server uses: `GET /info`, field `disk_blob_bytes`.
2. Check the limit. `--max-disk-bytes` (env `KEEP_MAX_DISK_BYTES`) caps the disk
   tier; unset or `0` means no limit.
3. Check the volume size in `k8s/base/statefulset.yaml`.
4. Free space by deleting keys, or grow the volume.
5. Check `GET /readyz` and do one test write.

Behavior on a full disk is not tested.

## Restore from backup

Create a backup. The destination is a `file://` URI, and `--retention-secs`
drops backups older than the given age:

```bash
keep backup --dest file:///var/backups/keep --data-dir ./data --shards 256 --retention-secs 604800
```

Restore into an empty data directory. Restore runs offline, so stop the server first:

```bash
keep restore --source file:///var/backups/keep --data-dir ./restored --shards 256
```

`--source` is either the directory (restore loads the newest
`backup-<unix seconds>.json` in it) or one exact file, such as
`file:///var/backups/keep/backup-123.json`. Restore refuses a non-empty
`--data-dir` unless `--force` is given, and it reads `file://` sources only;
copy `s3://` or `gs://` backups to a local file first. A backup keeps the expiry
time of each key; restore gives it back and skips a key that expired before the restore. A version 1 backup restores without expiry. Use the same `--shards` value as the
server. Then start the server with `--data-dir ./restored` and check `GET /readyz`
and the `keys` field of `GET /info`.

## Audit log

Turn it on with `KEEP_AUDIT_LOG=true` (or `--audit-log`) and restart. Each
data-plane write and each 401 or 403 makes one record. The records go to the
standard output of the keep process as `tracing` events with the target
`keep::audit`, and the fields `ts_unix_ms`, `event` (`write` or `auth_failure`),
`principal`, `operation`, `audit_target` and `result`. With
`KEEP_LOG_FORMAT=json` each record is one JSON line that a collector can read.
Filter them with `RUST_LOG=keep::audit=info`. A record never holds a value, a
query string or a token. A successful read is not recorded. Find them with
`kubectl logs statefulset/keep | grep keep::audit`.

## Token rotation

Keep reads `KEEP_API_TOKEN` and `KEEP_API_TOKEN_READONLY` once, at startup. There
is no hot reload, so a rotation needs a restart:

1. Create the new token values.
2. Update the environment or the Secret that holds them.
3. Restart the pods: `kubectl rollout restart statefulset/keep`.
4. Switch the clients to the new tokens.
5. Check that the old token gets `401` and the new token gets `200`. A read-only
   token on a write gets `403`.

`KEEP_TOKEN_SECRET` is separate: it enables scoped claim-check tokens for the
`/inputs/*` and `/results/*` routes.

To rotate `KEEP_TOKEN_SECRET` without rejecting a token that is still valid:

1. Set `KEEP_TOKEN_SECRET_PREVIOUS` to the old secret and `KEEP_TOKEN_SECRET` to
   the new secret, then restart the pods.
2. Re-issue the claim-check tokens with the new secret. Workers can keep using
   the old tokens until they expire.
3. Remove `KEEP_TOKEN_SECRET_PREVIOUS` and restart the pods.

A token signed by neither secret gets `403`.

## Upgrade

Use this procedure for a rolling upgrade of the `keep` StatefulSet.

1. Check the version pair against `docs/product/compatibility.md`. Release N+1
   reads every file that release N wrote, and replicas of N and N+1 use the
   same raft peer protocol. Downgrade is not supported: restore a backup taken
   before the upgrade (see `## Restore from backup`) instead.
2. Take a backup first: see [Restore from backup](#restore-from-backup).
3. Set the new image: `kubectl set image statefulset/keep keep=<new image>`.
   `k8s/base/statefulset.yaml` uses `updateStrategy: RollingUpdate` and
   `podManagementPolicy: OrderedReady`, so pods restart one at a time. The
   container name in that file is `keep`.
4. After each pod restarts, and before the next pod, continue only when both of
   these hold. Record them as two separate observations:
   - (a) readiness: `GET /readyz` returns `200 ok`;
   - (b) catch-up: `GET /shard/<id>/raftz` on the peer port of that pod shows
     `applied_index` equal to or close to `commit_index` of the leader, and the
     pod is not `leader: null`. Raft mode only; a single-node deployment has no
     catch-up step.
5. What is tested: `crates/keep/tests/it/rolling_restart.rs` restarts one replica at
   a time with the same version and checks that the restarted replica catches up
   and that no key is lost. `crates/keep/tests/it/rolling_upgrade.rs` upgrades a 3-process group one replica at a time from the previous build (`scripts/previous-keep-sha`) to the current build (`rolling_upgrade_previous_to_current_loses_no_key`) and rolls one replica back (`rolling_back_one_replica_loses_no_key`); both check that no key is lost and that writes continue.
6. Rollback: `kubectl rollout undo statefulset/keep` only when the release did not
   change the on-disk format (see `docs/product/compatibility.md`). After a format
   change, restore a backup.

The `kubectl` and `curl` lines in this section are not tested.

## Replica behind

1. Check `GET /shard/<id>/raftz` on the pod and on the leader. Read the fields
   `commit_index`, `applied_index`, `leader` and `role`.
2. A replica that is behind catches up from the leader. Check again after a while.
3. If `leader` stays `null`, check that a quorum of pods is up: see `## Node down`.

The `kubectl` and `curl` lines in this section are not tested.

## Readiness failing

1. Check `GET /readyz`. It returns `503 draining` during the drain window set by
   `--grace-secs` after SIGTERM. This is normal during a restart.
2. If it stays `503`, or the probe times out, read `kubectl describe pod <pod>`
   and `kubectl logs <pod>`, then see `## Node down`.

The `kubectl` and `curl` lines in this section are not tested.

## Token rejects

1. A client gets `401` (`unauthorized`: the token is missing or wrong) or `403`
   (a read-only token was used on a write).
2. Check the client's token against the Secret that holds the server tokens. See
   `## Token rotation`.

The `kubectl` and `curl` lines in this section are not tested.
