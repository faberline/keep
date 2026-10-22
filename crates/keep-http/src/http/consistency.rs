//! `x-read-consistency` on the data plane (raft replica mode).
//!
//! Every read on a replicated shard group carries one of raft-runtime's
//! [`ReadConsistency`] levels (`leader` when absent or unrecognized):
//!
//! | level          | served when                                         | else |
//! |----------------|-----------------------------------------------------|------|
//! | `leader`       | a barrier started after the request is applied here | 503 `read_fence_failed` |
//! | `bounded(ms)`  | this replica's staleness is at most `ms` (a leader that is over the bound runs a fence first) | 503 `replica_too_stale` |
//! | `any`          | always (may be stale)                               | —    |
//!
//! `leader` reads are linearizable on any replica: a follower's barrier is
//! forwarded to the leader and the follower answers once it has applied it.
//! `bounded` and `any` responses carry `x-keep-staleness-ms` when the replica
//! knows its bound. Writes, and nodes without replicas, pass through.
//!
//! [`ReadConsistency`]: crate::raft::ReadConsistency

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};

use crate::http::AppState;

/// Response header carrying the replica's staleness bound in ms.
pub const STALENESS_HEADER: &str = "x-keep-staleness-ms";

/// How long a read may wait for its barrier before answering 503.
#[cfg(feature = "raft")]
const READ_FENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Route-layer middleware enforcing the request's read consistency.
pub async fn enforce(State(st): State<AppState>, req: Request, next: Next) -> Response {
    #[cfg(feature = "raft")]
    {
        use axum::response::IntoResponse;

        if let Some(hosts) = st.raft_hosts.clone() {
            if hosts.replicated() && is_read(&req) {
                // `Request` is not `Sync`; read the header before awaiting.
                let level = crate::raft::ReadConsistency::from_header(
                    req.headers()
                        .get(crate::raft::READ_CONSISTENCY_HEADER)
                        .and_then(|v| v.to_str().ok()),
                );
                return match gate(&hosts, level).await {
                    Ok(staleness) => {
                        let mut resp = next.run(req).await;
                        if let Some(ms) = staleness {
                            resp.headers_mut().insert(STALENESS_HEADER, ms.into());
                        }
                        resp
                    }
                    Err(e) => e.into_response(),
                };
            }
        }
    }
    let _ = &st;
    next.run(req).await
}

/// Data-plane reads: every GET/HEAD plus the two POST batch reads.
#[cfg(feature = "raft")]
fn is_read(req: &Request) -> bool {
    use axum::{extract::MatchedPath, http::Method};

    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return true;
    }
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    *req.method() == Method::POST && matches!(path, Some("/kv:mget" | "/hashes/{key}/mget"))
}

/// Hold the request until its consistency level is met. `Ok(Some(ms))` is the
/// staleness bound to report on a `bounded`/`any` response.
#[cfg(feature = "raft")]
async fn gate(
    hosts: &crate::raft::ShardHosts,
    level: crate::raft::ReadConsistency,
) -> Result<Option<u64>, crate::http::error::ApiErr> {
    use crate::http::error::ApiErr;
    use crate::raft::ReadConsistency;
    use axum::http::StatusCode;

    let staleness_ms = || hosts.staleness().map(|d| d.as_millis() as u64);
    let fence = || async {
        match tokio::time::timeout(READ_FENCE_TIMEOUT, hosts.read_fence()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(ApiErr::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "read_fence_failed",
                format!("no leader could confirm this read: {e}"),
            )),
            Err(_) => Err(ApiErr::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "read_fence_failed",
                "no leader confirmed this read in time",
            )),
        }
    };
    match level {
        ReadConsistency::Leader => fence().await.map(|()| None),
        ReadConsistency::Any => Ok(staleness_ms()),
        ReadConsistency::Bounded(bound) => {
            if let Some(ms) = staleness_ms().filter(|ms| *ms <= bound) {
                return Ok(Some(ms));
            }
            if hosts.leads_all().await {
                fence().await?;
                return Ok(staleness_ms());
            }
            Err(ApiErr::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "replica_too_stale",
                match staleness_ms() {
                    Some(ms) => format!("replica is {ms}ms behind; bound is {bound}ms"),
                    None => format!("replica has not proved it is within {bound}ms"),
                },
            ))
        }
    }
}
