//! Claim-check handlers: job input and result payloads addressed by id (`/inputs/{id}`, `/results/{id}`).

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::http::error::{ApiErr, ApiError};
use crate::http::handlers::{ack_durable, key_of, propose_write, ttl, TtlQuery};
use crate::http::models::*;
use crate::http::AppState;
use crate::persistence::format::WalOp;
use crate::types::KvValue;

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
        if !propose_write(
            st,
            &k,
            WalOp::Set {
                key: k.as_str().to_string(),
                value: value.clone(),
                ttl,
            },
        )
        .await?
        {
            st.engine.set(&k, value, ttl).map_err(ApiErr::from)?;
        }
    } else {
        let value = KvValue::Bytes(body.to_vec());
        let ttl = ttl(q.ttl_ms);
        if !propose_write(
            st,
            &k,
            WalOp::Set {
                key: k.as_str().to_string(),
                value: value.clone(),
                ttl,
            },
        )
        .await?
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
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
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
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
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
