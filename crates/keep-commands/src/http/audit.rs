//! Audit log for the data plane (`KEEP_AUDIT_LOG`).
//!
//! When an audit sink is set, each write (any method but GET and HEAD) on a
//! data-plane route, and each 401 or 403, makes one [`AuditRecord`]. A record
//! has the time, the principal, the operation, the target name and the result.
//! It never holds a value, a query string or a token.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderMap, Method},
    middleware::Next,
    response::Response,
};
use serde::Serialize;
use service_auth::Verifier;

use crate::http::AppState;

/// One audit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRecord {
    /// Time of the request, in milliseconds since the Unix epoch.
    pub ts_unix_ms: u64,
    /// `auth_failure` for a 401 or 403, else `write`.
    pub event: &'static str,
    /// Who sent the request: `anonymous`, `api-token`, `read-only-token`,
    /// `claim-token` or `unauthenticated`.
    pub principal: &'static str,
    /// Method and matched route, for example `PUT /kv/{key}`.
    pub operation: String,
    /// The request path (no query string): the key or keyspace name.
    pub target: String,
    /// HTTP status code of the response.
    pub result: u16,
}

/// Receives each audit record. Set with `AppState::with_audit_sink`.
pub type AuditSink = Arc<dyn Fn(&AuditRecord) + Send + Sync>;

/// The default sink: one `tracing` event per record, target `keep::audit`.
pub fn tracing_sink() -> AuditSink {
    Arc::new(|r: &AuditRecord| {
        tracing::info!(
            target: "keep::audit",
            ts_unix_ms = r.ts_unix_ms,
            event = r.event,
            principal = r.principal,
            operation = %r.operation,
            audit_target = %r.target,
            result = r.result,
            "audit"
        );
    })
}

/// Layer on the data plane, outside `enforce_api_token`, so it sees the 401 and
/// 403 that the token check returns. Does nothing when no sink is set.
pub async fn audit(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let Some(sink) = st.audit.clone() else {
        return next.run(req).await;
    };
    let method = req.method().clone();
    let principal = principal(&st, req.headers(), req.uri().path());
    let operation = match req.extensions().get::<MatchedPath>() {
        Some(route) => format!("{method} {}", route.as_str()),
        None => format!("{method} <unmatched>"),
    };
    let target = req.uri().path().to_string();

    let resp = next.run(req).await;

    let result = resp.status().as_u16();
    let auth_failure = result == 401 || result == 403;
    let write = method != Method::GET && method != Method::HEAD;
    if auth_failure || write {
        sink(&AuditRecord {
            ts_unix_ms: unix_now_ms(),
            event: if auth_failure {
                "auth_failure"
            } else {
                "write"
            },
            principal,
            operation,
            target,
            result,
        });
    }
    resp
}

/// The principal, by the same bearer and claim-token rules as `enforce_api_token`.
/// The bearer text itself is never returned.
fn principal(st: &AppState, headers: &HeaderMap, path: &str) -> &'static str {
    if st.api_token.is_none() && st.read_only_token.is_none() {
        return "anonymous";
    }
    let bearer = service_auth::bearer_token(headers).unwrap_or("");
    if !bearer.is_empty() {
        if st.api_token.as_ref().is_some_and(|t| bearer == t.as_str()) {
            return "api-token";
        }
        if st
            .read_only_token
            .as_ref()
            .is_some_and(|t| bearer == t.as_str())
        {
            return "read-only-token";
        }
    }
    let is_claim_route = path.starts_with("/inputs/") || path.starts_with("/results/");
    if is_claim_route
        && st
            .verifier
            .as_ref()
            .is_some_and(|v| v.authenticate(headers).is_ok())
    {
        return "claim-token";
    }
    "unauthenticated"
}

/// Milliseconds since the Unix epoch, now.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
