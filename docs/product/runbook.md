# Keep data-plane runbook

This runbook covers the data plane of one Keep server: health and metrics, the
log format, and the procedures below. The `keep` commands here are checked against
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

To rotate `KEEP_TOKEN_SECRET` without rejecting a token that is still valid:

1. Set `KEEP_TOKEN_SECRET_PREVIOUS` to the old secret and `KEEP_TOKEN_SECRET` to
   the new secret, then restart the pods.
2. Re-issue the claim-check tokens with the new secret. Workers can keep using
   the old tokens until they expire.
3. Remove `KEEP_TOKEN_SECRET_PREVIOUS` and restart the pods.

A token signed by neither secret gets `403`.

## Upgrade

Upgrade one replica at a time, as `docs/product/compatibility.md` (section
`Upgrade order`) describes. Release N+1 reads every file that release N wrote,
and replicas of N and N+1 use the same raft peer protocol. Downgrade is not
supported: restore a backup taken before the upgrade instead
(`#restore-from-backup`).

1. Check the version pair against `docs/product/compatibility.md`.
2. Take a backup first: see [Restore from backup](#restore-from-backup).
3. Set the new image. The container is named `keep` in `k8s/base/statefulset.yaml`,
   which uses `updateStrategy: RollingUpdate` and `podManagementPolicy: OrderedReady`,
   so the pods restart one at a time:

   ```bash
   kubectl set image statefulset/keep keep=<new image>
   ```

4. After each pod restarts, and before the next pod, check two things and
   record each one separately. Continue only when both hold:
   - Readiness: `GET /readyz` returns `200 ok` on that pod.
   - Catch-up: `GET /shard/<id>/raftz` on that pod shows `applied_index` equal
     to, or close to, the leader's `commit_index`, and `leader` is not `null`.
     The raft routes share the HTTP port. A single-node deployment has no
     catch-up step.
5. What is tested: `crates/keep/tests/rolling_restart.rs` restarts one replica
   at a time with the same version. It checks that the restarted replica catches
   up and that no key is lost. A mixed-version rollback is not tested. A rolling
   upgrade across two versions is not tested.
6. Rollback: `kubectl rollout undo statefulset/keep` is safe only while the file
   formats did not change. A patch release never changes a file format
   (`docs/product/compatibility.md`, `On-disk format`). After a format change,
   restore a backup instead.

The `kubectl` lines in this section are not tested.

## Replica behind

1. Read `GET /shard/<id>/raftz` on the pod. Compare `commit_index` and
   `applied_index` with the same endpoint on the leader. The fields are
   `role`, `commit_index`, `applied_index` and `leader`.
2. A replica that is behind catches up from the leader. Wait for its
   `applied_index` to reach the leader's `commit_index`.
3. If `leader` stays `null`, check that a quorum of pods is up: see
   [Node down](#node-down).

The `kubectl` and `curl` lines in this section are not tested.

## Readiness failing

1. Check `GET /readyz` on the pod. `503 draining` is normal during the drain
   window `--grace-secs` after SIGTERM, so a restart shows it for a short time.
2. If it stays `503`, or the probe times out, read `kubectl describe pod <pod>`
   and `kubectl logs <pod>`, then follow [Node down](#node-down).

The `kubectl` and `curl` lines in this section are not tested.

## Token rejects

A client gets `401` with the code `unauthorized` when its token is missing or
wrong. A client gets `403` with the code `forbidden` when a read-only token
sends a write.

1. Compare the client's token with the Secret that holds it.
2. If the token was rotated, follow [Token rotation](#token-rotation).

The `curl` and `kubectl` lines in this section are not tested.
