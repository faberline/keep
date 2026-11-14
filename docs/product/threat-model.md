# keep threat model

This page lists what keep protects, where trust changes, and what is accepted. It describes the code at the current release and was written from `crates/keep-access/src/auth.rs`, `crates/keep/src/bin/keep/serve.rs`, `crates/keep/src/bin/keep/llm.rs` and `crates/keep-commands/src/http.rs`.

## Assets

- Stored values and claim-check payloads: `GET|PUT /inputs/{id}`, `GET|PUT /results/{id}`, and the key-value, collection and lock routes.
- `KEEP_API_TOKEN` (`--api-token`): a blanket Bearer token for data-plane routes. `KEEP_API_TOKEN_READONLY` (`--api-token-readonly`): a Bearer token that may only GET and HEAD data-plane routes; writes with it get 403. Both are optional; an empty value means off (`serve.rs`).
- `KEEP_TOKEN_SECRET`: the HMAC secret for scoped claim-check tokens. Optional; enforcement is on only when it is set and not empty (`serve.rs`, `auth.rs`).
- The data directory `KEEP_DATA_DIR` (default `./data`): the WAL, snapshots and tiered blobs on disk (`serve.rs`).
- Raft peer traffic on `/shard/{id}/raft/*` and `/shard/{id}/raftz` (`serve.rs`).
- TLS certificate and key files (`KEEP_TLS_CERT`, `KEEP_TLS_KEY`).

## Trust boundaries

| Boundary | Protocol | What crosses |
|---|---|---|
| Client to data plane | HTTP/1.1 and HTTP/2 cleartext (h2c) on one port, or HTTPS when `--tls-cert` and `--tls-key` are both set (`serve.rs`: they must be set together; otherwise startup fails). Default listen address `127.0.0.1:7117`. | Client requests and stored values. |
| Node to node (raft) | The peer routes share the serve port. They are merged into the same router with `app.merge(hosts.router())` in `serve.rs`, only in replica mode (`raft` feature and `REPLICAS_PER_SHARD` above 1). | Raft log entries and peer messages. |
| Operator to Kubernetes API | Crate `keep-operator` through `kube` 0.98.0 (`kube-client`). | Custom resources and cluster state. |

## Threats

| ID | Threat | Asset or boundary |
|---|---|---|
| T1 | Token theft (API tokens or a claim-check token) | Tokens; stored values |
| T2 | Replay of an expired claim-check token | Claim-check payloads |
| T3 | Scope escalation across keys | Stored values |
| T4 | Peer spoofing on the raft routes | Node to node boundary |
| T5 | Unencrypted transport | Client to data plane boundary |
| T6 | Disk theft (data directory) | Data directory, WAL, snapshots, blobs |
| T7 | Denial of service by large values | Data plane |

## Mitigations

- **T1: mitigated in part.** The read-only token limits what a stolen read token can do: writes get 403. Claim-check tokens are scoped to two bare ids and an expiry. Token rotation is not described by the code: `accepted`, with no rotation procedure yet (issue #115 adds the secret rotation test).
- **T2: mitigated.** `KeepVerifier::authenticate` in `auth.rs` calls `claim_token::verify(secret, token, now)` with the current unix time; an expired token (exp in the past) fails. The test `missing_or_invalid_or_expired_token_rejected` in `auth.rs` proves it. Every failure returns `403 Forbidden`, not `401`.
- **T3: mitigated in part.** `KeepPrincipal::authorizes(id, write)` in `auth.rs` compares `scope.w == id` for a write and `scope.r == id` for a read, on the bare url id before the `X-Keep-Namespace` prefix is applied. The test `out_of_scope_id_is_not_authorized` proves that a read key does not satisfy a write. Limit: claim-check enforcement covers only `GET /inputs/{id}` and `PUT /results/{id}`; `PUT` input and `GET` result stay open by design (`auth.rs` module doc).
- **T4: `accepted`.** Reason: keep adds no authentication of its own to the peer routes in `serve.rs`; they share the serve port. No NetworkPolicy is part of this project (`grep -rn NetworkPolicy k8s` finds none). The Service and the cluster network are the only boundary.
- **T5: mitigated when TLS is on; `accepted` otherwise.** HTTPS with `--tls-cert` and `--tls-key` mitigates it. The default is cleartext h2c, so deployments that do not set both options accept this threat.
- **T6: `accepted`.** Reason: keep does not encrypt the WAL, snapshots or blobs at rest. Use an encrypted volume.
- **T7: partly mitigated; `accepted` for request rate.** `KEEP_BODY_LIMIT` (`--body-limit`) bounds the request body; the default is 16 MiB (`DEFAULT_BODY_LIMIT = 16 * 1024 * 1024` in `crates/keep-commands/src/http.rs`). `KEEP_MAX_DISK_BYTES` bounds the disk tier. Many concurrent requests are not rate-limited by keep: `accepted`.

## Residual risk

- **T1, token rotation.** keep has no rotation procedure yet. Rotate the tokens by hand and keep the read-only token for clients that only read.
- **T4, peer routes.** Keep the raft port inside the cluster network; isolate it with network policy at the Service and cluster level.
- **T5, transport by default.** Set `--tls-cert` and `--tls-key` for any deployment that leaves the cluster.
- **T6, data at rest.** Store the data directory on an encrypted volume.
- **T7, request rate.** Put a rate limit in front of keep; keep does not rate-limit requests.

## Security review

- [ ] Independent security review by a person (not done by the flow)
