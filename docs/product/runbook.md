# Keep data-plane runbook

This runbook covers the data plane of one Keep server: health and metrics, the
log format, and four procedures. The `keep` commands here are checked against
the real CLI help by `crates/keep/tests/docs_lint.rs`. `curl` and `kubectl`
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
copy `s3://` or `gs://` backups to a local file first. TTLs are not part of a
backup, so restored keys have no expiry. Use the same `--shards` value as the
server. Then start the server with `--data-dir ./restored` and check `GET /readyz`
and the `keys` field of `GET /info`.

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
