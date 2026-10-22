//! Axum handlers. One function per route; each is annotated with
//! `#[utoipa::path]` so the generated OpenAPI document stays in lock-step with
//! the router.

use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::apply::ApplyOutcome;
use crate::cluster::ClusterState;
use crate::http::error::ApiErr;
use crate::http::models::*;
use crate::http::AppState;
use crate::persistence::format::WalOp;
use crate::types::{KvKey, KvValue};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub(crate) fn key_of(s: &str) -> Result<KvKey, ApiErr> {
    KvKey::new(s).map_err(ApiErr::from)
}

/// Returns true if `key` is in the reserved claim-check keyspace (`input:*`,
/// `result:*`, or namespaced `{ns}::input:*`, `{ns}::result:*`).
pub(crate) fn is_claim_check_key(key: &str) -> bool {
    let bare = match key.rsplit_once("::") {
        Some((_, suffix)) => suffix,
        None => key,
    };
    bare.starts_with("input:") || bare.starts_with("result:")
}

/// When scoped claim-check token enforcement (`KEEP_TOKEN_SECRET`) is active,
/// prevent callers from bypassing `/inputs/{id}` and `/results/{id}` scope
/// checks via raw `/kv/*` routes. When token enforcement is disabled, `/kv/*`
/// remains open for backward compatibility.
pub(crate) fn check_kv_claim_isolation(st: &AppState, key: &str) -> Result<(), ApiErr> {
    if st.verifier.is_some() && is_claim_check_key(key) {
        return Err(ApiErr::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "claim-check keys must be accessed via /inputs/{id} or /results/{id} when token enforcement is enabled",
        ));
    }
    Ok(())
}

pub(crate) fn ttl(ms: Option<u64>) -> Option<Duration> {
    ms.map(Duration::from_millis)
}

/// Await durability of the writes issued so far, then return — the basis for
/// durable-before-ack. No-op when persistence is disabled. Concurrent writers
/// share one fsync (group commit), so this is cheap under load.
pub(crate) async fn ack_durable(st: &AppState) {
    if let Some(rx) = st.engine.durability_barrier() {
        let _ = rx.await;
    }
}

/// In raft mode, replicate `op` through `key`'s shard group and return the
/// outcome the state machine computed when it applied the entry. `None` means
/// raft is off: the caller applies the write to the local engine itself.
pub(crate) async fn propose_write(
    st: &AppState,
    key: &KvKey,
    op: WalOp,
) -> Result<Option<ApplyOutcome>, ApiErr> {
    #[cfg(feature = "raft")]
    {
        if let Some(hosts) = &st.raft_hosts {
            if !hosts.owns(key.as_str()) {
                return Err(ApiErr::new(
                    StatusCode::CONFLICT,
                    "shard_not_owned",
                    format!(
                        "shard for key '{}' is not hosted by this node",
                        key.as_str()
                    ),
                ));
            }
            let outcome = hosts.write(key.as_str(), op).await.map_err(|e| {
                let msg = e.to_string();
                if msg.contains(crate::raft::OOM_REJECTION) {
                    ApiErr::new(StatusCode::INSUFFICIENT_STORAGE, "out_of_memory", msg)
                } else {
                    ApiErr::new(StatusCode::SERVICE_UNAVAILABLE, "raft_propose_failed", msg)
                }
            })?;
            return outcome.map(Some).map_err(ApiErr::from);
        }
    }
    let _ = (st, key, op);
    Ok(None)
}

/// In raft mode, refuse a batch whose keys span several shard groups: each
/// group commits independently, so the batch could not apply atomically.
pub(crate) fn require_single_shard(st: &AppState, keys: &[KvKey]) -> Result<(), ApiErr> {
    #[cfg(feature = "raft")]
    {
        if let Some(hosts) = &st.raft_hosts {
            let mut shards = keys.iter().map(|k| hosts.shard_for(k.as_str()));
            if let Some(first) = shards.next() {
                if shards.any(|s| s != first) {
                    return Err(ApiErr::new(
                        StatusCode::BAD_REQUEST,
                        "cross_shard_batch",
                        "batch keys span several shard groups; split the batch by shard",
                    ));
                }
            }
        }
    }
    let _ = (st, keys);
    Ok(())
}

fn unexpected_outcome(o: &ApplyOutcome) -> ApiErr {
    ApiErr::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected_outcome",
        format!("raft apply returned an unexpected outcome: {o:?}"),
    )
}

pub(crate) fn outcome_bool(o: ApplyOutcome) -> Result<bool, ApiErr> {
    o.as_bool().ok_or_else(|| unexpected_outcome(&o))
}

pub(crate) fn outcome_int(o: ApplyOutcome) -> Result<i64, ApiErr> {
    o.as_int().ok_or_else(|| unexpected_outcome(&o))
}

pub(crate) fn outcome_count(o: ApplyOutcome) -> Result<usize, ApiErr> {
    o.as_count().ok_or_else(|| unexpected_outcome(&o))
}

pub(crate) fn outcome_float(o: ApplyOutcome) -> Result<f64, ApiErr> {
    o.as_float().ok_or_else(|| unexpected_outcome(&o))
}

pub(crate) fn outcome_value(o: ApplyOutcome) -> Result<Option<KvValue>, ApiErr> {
    let err = unexpected_outcome(&o);
    o.into_value().ok_or(err)
}

/// TTL passed via query string on the raw-bytes write path.
#[derive(Debug, Deserialize)]
pub struct TtlQuery {
    pub ttl_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// single-key
// ---------------------------------------------------------------------------

/// Fetch a value. Byte blobs are returned as `application/octet-stream`;
/// everything else as a JSON `ValueResponse`.
#[utoipa::path(
    get,
    path = "/kv/{key}",
    tag = "KV",
    params(("key" = String, Path, description = "Key to fetch")),
    responses(
        (status = 200, description = "Value found", body = ValueResponse),
        (status = 404, description = "Key not found", body = ApiError)
    )
)]
pub async fn get_key(
    State(st): State<AppState>,
    Path(key): Path<String>,
) -> Result<Response, ApiErr> {
    check_kv_claim_isolation(&st, &key)?;
    let k = key_of(&key)?;
    match st.engine.get(&k) {
        None => Err(ApiErr::new(
            StatusCode::NOT_FOUND,
            "key_not_found",
            format!("key not found: {key}"),
        )),
        Some(KvValue::Bytes(b)) => {
            Ok(([(CONTENT_TYPE, "application/octet-stream")], b).into_response())
        }
        Some(v) => Ok(Json(ValueResponse {
            key,
            value: kv_to_json(v),
        })
        .into_response()),
    }
}

/// Store a value. `application/json` bodies are a `SetRequest`; an
/// `application/octet-stream` body is stored verbatim as a byte blob (TTL via
/// `?ttl_ms=`), which is the efficient path for claim-check payloads.
#[utoipa::path(
    put,
    path = "/kv/{key}",
    tag = "KV",
    params(("key" = String, Path, description = "Key to set")),
    request_body(content = SetRequest, description = "JSON value + optional ttl_ms (or send a raw octet-stream blob)"),
    responses(
        (status = 200, description = "Stored", body = OkResponse),
        (status = 400, description = "Invalid key or body", body = ApiError),
        (status = 415, description = "Unsupported content-type", body = ApiError),
        (status = 507, description = "Out of memory", body = ApiError)
    )
)]
pub async fn put_key(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<TtlQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiErr> {
    check_kv_claim_isolation(&st, &key)?;
    let k = key_of(&key)?;
    let ct = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    if ct.starts_with("application/octet-stream") {
        let value = KvValue::Bytes(body.to_vec());
        let ttl = ttl(q.ttl_ms);
        if propose_write(
            &st,
            &k,
            WalOp::Set {
                key: k.as_str().to_string(),
                value: value.clone(),
                ttl,
            },
        )
        .await?
        .is_none()
        {
            st.engine.set(&k, value, ttl).map_err(ApiErr::from)?;
        }
    } else if ct.starts_with("application/json") {
        // Fast parse: JSON tokens -> KvValue directly, no serde_json::Value tree
        // (the measured write-path hot cost). SetRequest stays the OpenAPI schema.
        let req: SetRequestFast = serde_json::from_slice(&body)
            .map_err(|e| ApiErr::bad_request(format!("invalid JSON body: {e}")))?;
        let value = req.value.0;
        let ttl = ttl(req.ttl_ms);
        if propose_write(
            &st,
            &k,
            WalOp::Set {
                key: k.as_str().to_string(),
                value: value.clone(),
                ttl,
            },
        )
        .await?
        .is_none()
        {
            st.engine.set(&k, value, ttl).map_err(ApiErr::from)?;
        }
    } else {
        return Err(ApiErr::unsupported_media_type(format!(
            "unsupported content-type: {ct}"
        )));
    }
    ack_durable(&st).await;
    Ok((StatusCode::OK, Json(OkResponse { key, ok: true })).into_response())
}

/// Delete a key.
#[utoipa::path(
    delete,
    path = "/kv/{key}",
    tag = "KV",
    params(("key" = String, Path, description = "Key to delete")),
    responses((status = 200, description = "Delete result", body = DeleteResponse))
)]
pub async fn delete_key(
    State(st): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<DeleteResponse>, ApiErr> {
    check_kv_claim_isolation(&st, &key)?;
    let k = key_of(&key)?;
    let deleted = match propose_write(
        &st,
        &k,
        WalOp::Delete {
            key: k.as_str().to_string(),
        },
    )
    .await?
    {
        Some(o) => outcome_bool(o)?,
        None => st.engine.delete(&k),
    };
    ack_durable(&st).await;
    Ok(Json(DeleteResponse { deleted }))
}

/// Existence check (no body).
#[utoipa::path(
    head,
    path = "/kv/{key}",
    tag = "KV",
    params(("key" = String, Path, description = "Key to test")),
    responses(
        (status = 200, description = "Key exists"),
        (status = 404, description = "Key absent")
    )
)]
pub async fn head_key(State(st): State<AppState>, Path(key): Path<String>) -> StatusCode {
    if check_kv_claim_isolation(&st, &key).is_err() {
        return StatusCode::FORBIDDEN;
    }
    match key_of(&key) {
        Ok(k) if st.engine.exists(&k) => StatusCode::OK,
        Ok(_) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

/// Atomic increment/decrement of an integer value.
#[utoipa::path(
    post,
    path = "/kv/{key}/incr",
    tag = "KV",
    params(("key" = String, Path, description = "Counter key")),
    request_body = IncrRequest,
    responses(
        (status = 200, description = "New value", body = IncrResponse),
        (status = 409, description = "Existing value is not an integer", body = ApiError)
    )
)]
pub async fn incr_key(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<IncrRequest>,
) -> Result<Json<IncrResponse>, ApiErr> {
    check_kv_claim_isolation(&st, &key)?;
    let k = key_of(&key)?;
    let value = match propose_write(
        &st,
        &k,
        WalOp::Incr {
            key: k.as_str().to_string(),
            delta: req.delta,
        },
    )
    .await?
    {
        Some(o) => outcome_int(o)?,
        None => st.engine.incr(&k, req.delta).map_err(ApiErr::from)?,
    };
    ack_durable(&st).await;
    Ok(Json(IncrResponse { value }))
}

/// Compare-and-swap.
#[utoipa::path(
    post,
    path = "/kv/{key}/cas",
    tag = "KV",
    params(("key" = String, Path, description = "Key to swap")),
    request_body = CasRequest,
    responses((status = 200, description = "Swap result", body = CasResponse))
)]
pub async fn cas_key(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<CasRequest>,
) -> Result<Json<CasResponse>, ApiErr> {
    check_kv_claim_isolation(&st, &key)?;
    let k = key_of(&key)?;
    let expected = json_to_kv(req.expected);
    let new = json_to_kv(req.new);
    let ttl = ttl(req.ttl_ms);
    let swapped = match propose_write(
        &st,
        &k,
        WalOp::Cas {
            key: k.as_str().to_string(),
            expected: expected.clone(),
            new: new.clone(),
            ttl,
        },
    )
    .await?
    {
        Some(o) => outcome_bool(o)?,
        None => st
            .engine
            .cas(&k, &expected, new, ttl)
            .map_err(ApiErr::from)?,
    };
    ack_durable(&st).await;
    Ok(Json(CasResponse { swapped }))
}

/// Set only if the key does not already exist.
#[utoipa::path(
    post,
    path = "/kv/{key}/setnx",
    tag = "KV",
    params(("key" = String, Path, description = "Key to set")),
    request_body = SetRequest,
    responses((status = 200, description = "Set result", body = SetNxResponse))
)]
pub async fn setnx_key(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<SetRequest>,
) -> Result<Json<SetNxResponse>, ApiErr> {
    check_kv_claim_isolation(&st, &key)?;
    let k = key_of(&key)?;
    let value = json_to_kv(req.value);
    let ttl = ttl(req.ttl_ms);
    let set = match propose_write(
        &st,
        &k,
        WalOp::SetNx {
            key: k.as_str().to_string(),
            value: value.clone(),
            ttl,
        },
    )
    .await?
    {
        Some(o) => outcome_bool(o)?,
        None => st.engine.setnx(&k, value, ttl).map_err(ApiErr::from)?,
    };
    ack_durable(&st).await;
    Ok(Json(SetNxResponse { set }))
}

// ---------------------------------------------------------------------------
// batch
// ---------------------------------------------------------------------------

/// Fetch many keys at once.
#[utoipa::path(
    post,
    path = "/kv:mget",
    tag = "Batch",
    request_body = MGetRequest,
    responses((status = 200, description = "Values parallel to the request keys", body = MGetResponse))
)]
pub async fn mget(
    State(st): State<AppState>,
    Json(req): Json<MGetRequest>,
) -> Result<Json<MGetResponse>, ApiErr> {
    for s in &req.keys {
        check_kv_claim_isolation(&st, s)?;
    }
    let keys: Vec<KvKey> = req
        .keys
        .iter()
        .map(|s| KvKey::new(s))
        .collect::<Result<_, _>>()
        .map_err(ApiErr::from)?;
    let refs: Vec<&KvKey> = keys.iter().collect();
    let values = st
        .engine
        .mget(&refs)
        .into_iter()
        .map(|o| o.map(kv_to_json))
        .collect();
    Ok(Json(MGetResponse { values }))
}

/// Set many keys at once with a shared TTL.
#[utoipa::path(
    post,
    path = "/kv:mset",
    tag = "Batch",
    request_body = MSetRequest,
    responses((status = 200, description = "Number of keys written", body = CountResponse))
)]
pub async fn mset(State(st): State<AppState>, body: Bytes) -> Result<Json<CountResponse>, ApiErr> {
    // Fast parse: entry values -> KvValue directly (no serde_json::Value each).
    let req: MSetRequestFast = serde_json::from_slice(&body)
        .map_err(|e| ApiErr::bad_request(format!("invalid JSON body: {e}")))?;
    let mut keys = Vec::with_capacity(req.entries.len());
    let mut vals = Vec::with_capacity(req.entries.len());
    for (k, v) in req.entries {
        check_kv_claim_isolation(&st, &k)?;
        keys.push(KvKey::new(&k).map_err(ApiErr::from)?);
        vals.push(v.0);
    }
    let ttl = ttl(req.ttl_ms);
    require_single_shard(&st, &keys)?;
    let count = keys.len();
    let replicated = match keys.first() {
        Some(first) => propose_write(
            &st,
            first,
            WalOp::MSet {
                pairs: keys
                    .iter()
                    .map(|k| k.as_str().to_string())
                    .zip(vals.iter().cloned())
                    .collect(),
                ttl,
            },
        )
        .await?
        .is_some(),
        None => false,
    };
    if !replicated {
        let pairs: Vec<(&KvKey, KvValue)> = keys.iter().zip(vals).collect();
        st.engine.mset(&pairs, ttl).map_err(ApiErr::from)?;
    }
    ack_durable(&st).await;
    Ok(Json(CountResponse { count }))
}

/// Delete many keys at once.
#[utoipa::path(
    post,
    path = "/kv:mdel",
    tag = "Batch",
    request_body = MDelRequest,
    responses((status = 200, description = "Number of keys deleted", body = CountResponse))
)]
pub async fn mdel(
    State(st): State<AppState>,
    Json(req): Json<MDelRequest>,
) -> Result<Json<CountResponse>, ApiErr> {
    for s in &req.keys {
        check_kv_claim_isolation(&st, s)?;
    }
    let keys: Vec<KvKey> = req
        .keys
        .iter()
        .map(|s| KvKey::new(s))
        .collect::<Result<_, _>>()
        .map_err(ApiErr::from)?;
    require_single_shard(&st, &keys)?;
    let replicated = match keys.first() {
        Some(first) => {
            propose_write(
                &st,
                first,
                WalOp::MDel {
                    keys: keys.iter().map(|k| k.as_str().to_string()).collect(),
                },
            )
            .await?
        }
        None => None,
    };
    let count = match replicated {
        Some(o) => outcome_count(o)?,
        None => {
            let refs: Vec<&KvKey> = keys.iter().collect();
            st.engine.mdel(&refs)
        }
    };
    ack_durable(&st).await;
    Ok(Json(CountResponse { count }))
}

/// List keys, optionally filtered by prefix.
#[utoipa::path(
    get,
    path = "/kv",
    tag = "KV",
    params(
        ("prefix" = Option<String>, Query, description = "Only keys with this prefix"),
        ("limit" = Option<usize>, Query, description = "Max keys to return (default 100)")
    ),
    responses((status = 200, description = "Matching keys", body = ScanResponse))
)]
pub async fn scan(State(st): State<AppState>, Query(q): Query<ScanQuery>) -> Json<ScanResponse> {
    let mut keys = st.engine.scan(q.prefix.as_deref(), q.limit);
    if st.verifier.is_some() {
        keys.retain(|k| !is_claim_check_key(k));
    }
    Json(ScanResponse { keys })
}

// ---------------------------------------------------------------------------
// locks
// ---------------------------------------------------------------------------

/// Acquire a leased lock. Idempotent for the same owner.
#[utoipa::path(
    post,
    path = "/locks/{key}",
    tag = "Locks",
    params(("key" = String, Path, description = "Lock name")),
    request_body = LockRequest,
    responses((status = 200, description = "Acquire result", body = LockResponse))
)]
pub async fn lock(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<LockRequest>,
) -> Result<Json<LockResponse>, ApiErr> {
    let k = key_of(&key)?;
    let ttl = Duration::from_millis(req.ttl_ms);
    let acquired = match propose_write(
        &st,
        &k,
        WalOp::Lock {
            key: k.as_str().to_string(),
            owner: req.owner.clone(),
            ttl,
        },
    )
    .await?
    {
        Some(o) => outcome_bool(o)?,
        None => st.engine.lock(&k, &req.owner, ttl),
    };
    ack_durable(&st).await;
    Ok(Json(LockResponse { acquired }))
}

/// Release a lock held by `owner`.
#[utoipa::path(
    delete,
    path = "/locks/{key}",
    tag = "Locks",
    params(("key" = String, Path, description = "Lock name")),
    request_body = UnlockRequest,
    responses(
        (status = 200, description = "Release result", body = UnlockResponse),
        (status = 409, description = "Lock held by a different owner", body = ApiError)
    )
)]
pub async fn unlock(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<UnlockRequest>,
) -> Result<Json<UnlockResponse>, ApiErr> {
    let k = key_of(&key)?;
    let released = match propose_write(
        &st,
        &k,
        WalOp::Unlock {
            key: k.as_str().to_string(),
            owner: req.owner.clone(),
        },
    )
    .await?
    {
        Some(o) => outcome_bool(o)?,
        None => st.engine.unlock(&k, &req.owner).map_err(ApiErr::from)?,
    };
    ack_durable(&st).await;
    Ok(Json(UnlockResponse { released }))
}

/// Extend the lease on a held lock.
#[utoipa::path(
    patch,
    path = "/locks/{key}",
    tag = "Locks",
    params(("key" = String, Path, description = "Lock name")),
    request_body = ExtendLockRequest,
    responses(
        (status = 200, description = "Extend result", body = ExtendLockResponse),
        (status = 409, description = "Lock not held or owner mismatch", body = ApiError)
    )
)]
pub async fn extend_lock(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<ExtendLockRequest>,
) -> Result<Json<ExtendLockResponse>, ApiErr> {
    let k = key_of(&key)?;
    let ttl = Duration::from_millis(req.ttl_ms);
    let extended = match propose_write(
        &st,
        &k,
        WalOp::ExtendLock {
            key: k.as_str().to_string(),
            owner: req.owner.clone(),
            ttl,
        },
    )
    .await?
    {
        Some(o) => outcome_bool(o)?,
        None => st
            .engine
            .extend_lock(&k, &req.owner, ttl)
            .map_err(ApiErr::from)?,
    };
    ack_durable(&st).await;
    Ok(Json(ExtendLockResponse { extended }))
}

// ---------------------------------------------------------------------------
// lists
// ---------------------------------------------------------------------------

/// Prepend values to a list (LPUSH).
#[utoipa::path(
    post,
    path = "/lists/{key}/lpush",
    tag = "Lists",
    params(("key" = String, Path, description = "List key")),
    request_body = PushRequest,
    responses((status = 200, description = "New list length", body = PushResponse))
)]
pub async fn lpush(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<PushRequest>,
) -> Result<Json<PushResponse>, ApiErr> {
    let k = key_of(&key)?;
    let values: Vec<KvValue> = req.values.into_iter().map(json_to_kv).collect();
    let length = match propose_write(
        &st,
        &k,
        WalOp::LPush {
            key: k.as_str().to_string(),
            values: values.clone(),
        },
    )
    .await?
    {
        Some(o) => outcome_count(o)?,
        None => st.engine.lpush(&k, values).map_err(ApiErr::from)?,
    };
    ack_durable(&st).await;
    st.waiters.notify(&key);
    Ok(Json(PushResponse { length }))
}

/// Append values to a list (RPUSH).
#[utoipa::path(
    post,
    path = "/lists/{key}/rpush",
    tag = "Lists",
    params(("key" = String, Path, description = "List key")),
    request_body = PushRequest,
    responses((status = 200, description = "New list length", body = PushResponse))
)]
pub async fn rpush(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<PushRequest>,
) -> Result<Json<PushResponse>, ApiErr> {
    let k = key_of(&key)?;
    let values: Vec<KvValue> = req.values.into_iter().map(json_to_kv).collect();
    let length = match propose_write(
        &st,
        &k,
        WalOp::RPush {
            key: k.as_str().to_string(),
            values: values.clone(),
        },
    )
    .await?
    {
        Some(o) => outcome_count(o)?,
        None => st.engine.rpush(&k, values).map_err(ApiErr::from)?,
    };
    ack_durable(&st).await;
    st.waiters.notify(&key);
    Ok(Json(PushResponse { length }))
}

/// Pop from the head of a list (LPOP).
#[utoipa::path(
    post,
    path = "/lists/{key}/lpop",
    tag = "Lists",
    params(("key" = String, Path, description = "List key")),
    responses((status = 200, description = "Popped value or null", body = PopResponse))
)]
pub async fn lpop(
    State(st): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<PopResponse>, ApiErr> {
    let k = key_of(&key)?;
    let value = match propose_write(
        &st,
        &k,
        WalOp::LPop {
            key: k.as_str().to_string(),
        },
    )
    .await?
    {
        Some(o) => outcome_value(o)?,
        None => st.engine.lpop(&k),
    };
    ack_durable(&st).await;
    Ok(Json(PopResponse {
        value: value.map(kv_to_json),
    }))
}

/// Pop from the tail of a list (RPOP).
#[utoipa::path(
    post,
    path = "/lists/{key}/rpop",
    tag = "Lists",
    params(("key" = String, Path, description = "List key")),
    responses((status = 200, description = "Popped value or null", body = PopResponse))
)]
pub async fn rpop(
    State(st): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<PopResponse>, ApiErr> {
    let k = key_of(&key)?;
    let value = match propose_write(
        &st,
        &k,
        WalOp::RPop {
            key: k.as_str().to_string(),
        },
    )
    .await?
    {
        Some(o) => outcome_value(o)?,
        None => st.engine.rpop(&k),
    };
    ack_durable(&st).await;
    Ok(Json(PopResponse {
        value: value.map(kv_to_json),
    }))
}

// ---------------------------------------------------------------------------
// admin / probes
// ---------------------------------------------------------------------------

/// Liveness probe — process is up. Always 200 unless the process is wedged.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "Admin",
    responses((status = 200, description = "Alive", body = String))
)]
pub async fn healthz() -> &'static str {
    "ok"
}

/// Readiness probe — 200 when serving, 503 while draining on SIGTERM so k8s
/// stops routing traffic before the pod exits.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "Admin",
    responses(
        (status = 200, description = "Ready"),
        (status = 503, description = "Draining / not ready")
    )
)]
pub async fn readyz(State(st): State<AppState>) -> (StatusCode, &'static str) {
    if st.is_draining() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining")
    } else {
        (StatusCode::OK, "ok")
    }
}

/// Prometheus text-format metrics.
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Admin",
    responses((status = 200, description = "Prometheus exposition", body = String))
)]
pub async fn metrics(
    State(st): State<AppState>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    use service_http::MetricsProvider;
    let body = st.render_metrics();
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

/// Server info snapshot.
#[utoipa::path(
    get,
    path = "/info",
    tag = "Admin",
    responses((status = 200, description = "Server info", body = InfoResponse))
)]
pub async fn info(State(st): State<AppState>) -> Json<InfoResponse> {
    Json(InfoResponse {
        keys: st.engine.len(),
        shards: st.engine.num_shards(),
        memory_bytes: st.engine.estimate_memory(),
        maxmemory_bytes: st.engine.get_maxmemory(),
        disk_blob_bytes: st.engine.used_disk_bytes(),
        evicted_keys: st.engine.evicted_keys(),
        spilled_keys: st.engine.spilled_keys(),
        eviction_policy: st.engine.get_eviction_policy().as_str().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// Cluster topology + sharding view. Clients route a key with
/// `crc32(key) % shard_count -> shard -> node` (see the HA / Raft capability).
#[utoipa::path(
    get,
    path = "/cluster",
    tag = "Admin",
    responses((status = 200, description = "Cluster topology", body = ClusterState))
)]
pub async fn cluster(State(st): State<AppState>) -> Json<ClusterState> {
    Json(st.cluster.snapshot())
}

/// The generated OpenAPI document.
#[utoipa::path(get, path = "/openapi.json", tag = "Admin", responses((status = 200, description = "OpenAPI 3 document")))]
pub async fn openapi_spec() -> Json<utoipa::openapi::OpenApi> {
    use utoipa::OpenApi;
    Json(crate::http::openapi::ApiDoc::openapi())
}

/// Minimal API docs page (Redoc, loads `/openapi.json`).
pub async fn docs() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html>
  <head>
    <title>keep API</title>
    <meta charset="utf-8"/>
    <meta name="viewport" content="width=device-width, initial-scale=1"/>
  </head>
  <body>
    <redoc spec-url="/openapi.json"></redoc>
    <script src="https://cdn.redoc.ly/redoc/latest/bundles/redoc.standalone.js"></script>
  </body>
</html>"#,
    )
}

// ---------------------------------------------------------------------------
// Claim-check API (#114 / #167): job input/result payloads addressed by id.
// Thin aliases over the KV store (namespaced `input:` / `result:` keys),
// bytes-first. This is the contract relay's worker-protocol references — a
// worker GETs its input and PUTs its result by message id; the producer is the
// mirror (PUT input, GET result).
// ---------------------------------------------------------------------------

/// Per-tenant storage key for a claim-check payload: bare `{kind}:{id}` with no
/// namespace, or `{ns}::{kind}:{id}` when `X-Keep-Namespace` is set (#464). The
/// namespace is applied here — AFTER the token scope check, which keys on the
/// bare URL id — so the token scope stays bare per loom's settled design.
fn claim_storage_key(ns: &str, kind: &str, id: &str) -> String {
    if ns.is_empty() {
        format!("{kind}:{id}")
    } else {
        format!("{ns}::{kind}:{id}")
    }
}

/// Extract loom's `X-Keep-Namespace` header (sent from `LOOM_NAMESPACE`); empty
/// when absent, which keeps the bare key for single-tenant callers.
fn claim_namespace(headers: &HeaderMap) -> String {
    headers
        .get("x-keep-namespace")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

async fn claim_get(st: &AppState, ns: &str, kind: &str, id: &str) -> Result<Response, ApiErr> {
    let k = key_of(&claim_storage_key(ns, kind, id))?;
    match st.engine.get(&k) {
        None => Err(ApiErr::new(
            StatusCode::NOT_FOUND,
            "claim_check_not_found",
            format!("{kind} not found: {id}"),
        )),
        Some(KvValue::Bytes(b)) => {
            Ok(([(CONTENT_TYPE, "application/octet-stream")], b).into_response())
        }
        Some(v) => Ok(Json(ValueResponse {
            key: id.to_string(),
            value: kv_to_json(v),
        })
        .into_response()),
    }
}

async fn claim_put(
    st: &AppState,
    ns: &str,
    kind: &str,
    id: &str,
    q: TtlQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiErr> {
    let k = key_of(&claim_storage_key(ns, kind, id))?;
    let ct = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");
    if ct.starts_with("application/json") {
        let req: SetRequestFast = serde_json::from_slice(&body)
            .map_err(|e| ApiErr::bad_request(format!("invalid JSON body: {e}")))?;
        let value = req.value.0;
        let ttl = ttl(req.ttl_ms);
        if propose_write(
            st,
            &k,
            WalOp::Set {
                key: k.as_str().to_string(),
                value: value.clone(),
                ttl,
            },
        )
        .await?
        .is_none()
        {
            st.engine.set(&k, value, ttl).map_err(ApiErr::from)?;
        }
    } else {
        let value = KvValue::Bytes(body.to_vec());
        let ttl = ttl(q.ttl_ms);
        if propose_write(
            st,
            &k,
            WalOp::Set {
                key: k.as_str().to_string(),
                value: value.clone(),
                ttl,
            },
        )
        .await?
        .is_none()
        {
            st.engine.set(&k, value, ttl).map_err(ApiErr::from)?;
        }
    }
    ack_durable(st).await;
    Ok((
        StatusCode::OK,
        Json(OkResponse {
            key: id.to_string(),
            ok: true,
        }),
    )
        .into_response())
}

/// Fetch a job input by id (worker reads its input — #167).
#[utoipa::path(
    get, path = "/inputs/{id}", tag = "Claim-check",
    params(("id" = String, Path, description = "Job input id")),
    responses(
        (status = 200, description = "Input payload"),
        (status = 404, description = "Not found", body = ApiError)
    )
)]
/// Enforce a scoped claim-check token on worker ops (#446, #746). No-op when
/// token enforcement is off (`AppState.verifier` is `None`). `write` = PUT
/// result (scope.w); else GET input (scope.r).
///
/// Delegates bearer extraction + token verification to keep's shared
/// `service_auth::Verifier` (`auth::KeepVerifier`, which composes
/// `libs/claim-token`) and keeps the per-resource scope decision here: the token
/// scope is matched against the BARE url `id`, before the `X-Keep-Namespace`
/// prefix is applied. Every failure — missing, invalid, expired, or
/// out-of-scope — maps to the same `403 Forbidden` keep already returns; the
/// blanket `service_auth::auth_middleware` (which renders 401 and gates whole
/// routers) deliberately is NOT used, to preserve this contract.
fn check_scope(st: &AppState, headers: &HeaderMap, id: &str, write: bool) -> Result<(), ApiErr> {
    use service_auth::Verifier;
    let Some(verifier) = &st.verifier else {
        return Ok(());
    };
    match verifier.authenticate(headers) {
        Ok(principal) if principal.authorizes(id, write) => Ok(()),
        _ => Err(ApiErr::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "missing, invalid, expired, or out-of-scope claim-check token",
        )),
    }
}

pub async fn get_input(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiErr> {
    check_scope(&st, &headers, &id, false)?;
    let ns = claim_namespace(&headers);
    claim_get(&st, &ns, "input", &id).await
}

/// Store a job input by id (producer writes the input — #167).
#[utoipa::path(
    put, path = "/inputs/{id}", tag = "Claim-check",
    params(("id" = String, Path, description = "Job input id")),
    responses((status = 200, description = "Stored", body = OkResponse))
)]
pub async fn put_input(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TtlQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiErr> {
    let ns = claim_namespace(&headers);
    claim_put(&st, &ns, "input", &id, q, headers, body).await
}

/// Fetch a job result by id (producer reads the result — #167).
#[utoipa::path(
    get, path = "/results/{id}", tag = "Claim-check",
    params(("id" = String, Path, description = "Job result id")),
    responses(
        (status = 200, description = "Result payload"),
        (status = 404, description = "Not found", body = ApiError)
    )
)]
pub async fn get_result(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiErr> {
    let ns = claim_namespace(&headers);
    claim_get(&st, &ns, "result", &id).await
}

/// Store a job result by id (worker writes its result — #167).
#[utoipa::path(
    put, path = "/results/{id}", tag = "Claim-check",
    params(("id" = String, Path, description = "Job result id")),
    responses((status = 200, description = "Stored", body = OkResponse))
)]
pub async fn put_result(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TtlQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiErr> {
    check_scope(&st, &headers, &id, true)?;
    let ns = claim_namespace(&headers);
    claim_put(&st, &ns, "result", &id, q, headers, body).await
}
