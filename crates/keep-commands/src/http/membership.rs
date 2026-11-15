//! Dynamic raft membership admin API (keep#30): add a node to a shard group as
//! a learner, promote a caught-up learner to voter, and remove a member. No
//! restart. A refused request is a 4xx with a stable code and changes nothing.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::http::error::ApiErr;
use crate::http::AppState;

/// Body of `POST /admin/raft/{shard}/members`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AddLearnerRequest {
    /// Raft node id of the new member, its replica index in the shard group.
    pub node_id: u64,
    /// Peer base URL of the new member, including its `/shard/{shard}` prefix,
    /// for example `http://10.0.0.3:7117/shard/0`. Must start with `http://` or `https://`.
    pub url: String,
}

/// Result of a membership change.
#[derive(Debug, Serialize, ToSchema)]
pub struct MembershipResponse {
    /// The shard group that changed.
    pub shard: u32,
    /// The raft node id that was added, promoted or removed.
    pub node_id: u64,
    /// The raft log index of the membership entry.
    pub index: u64,
}

fn raft_disabled() -> ApiErr {
    ApiErr::new(
        StatusCode::NOT_FOUND,
        "raft_disabled",
        "this node does not run raft replica mode",
    )
}

#[cfg(feature = "raft")]
fn shard_not_owned(shard: u32) -> ApiErr {
    ApiErr::new(
        StatusCode::CONFLICT,
        "shard_not_owned",
        format!("shard {shard} is not hosted by this node"),
    )
}

/// Map a membership refusal to its status. A refusal is never a 5xx.
#[cfg(feature = "raft")]
fn refusal(err: crate::raft::MembershipError) -> ApiErr {
    use crate::raft::MembershipError;
    let status = match &err {
        MembershipError::NotFound(_) => StatusCode::NOT_FOUND,
        MembershipError::Unroutable(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::CONFLICT,
    };
    ApiErr::new(status, err.code(), err.to_string())
}

fn valid_peer_url(url: &str) -> bool {
    !url.is_empty() && (url.starts_with("http://") || url.starts_with("https://"))
}

/// Add `node_id` to shard `shard`'s group as a learner reachable at `url`.
#[utoipa::path(
    post,
    path = "/admin/raft/{shard}/members",
    tag = "Admin",
    params(("shard" = u32, Path, description = "Shard group id")),
    request_body = AddLearnerRequest,
    responses(
        (status = 200, description = "Learner admitted", body = MembershipResponse),
        (status = 400, description = "Invalid url or unroutable peer", body = ApiError),
        (status = 404, description = "Raft disabled on this node", body = ApiError),
        (status = 409, description = "Shard not hosted here, or a change is refused", body = ApiError)
    )
)]
pub async fn add_learner(
    State(st): State<AppState>,
    Path(shard): Path<u32>,
    Json(req): Json<AddLearnerRequest>,
) -> Result<Json<MembershipResponse>, ApiErr> {
    if !valid_peer_url(&req.url) {
        return Err(ApiErr::bad_request(
            "url must start with http:// or https://",
        ));
    }
    #[cfg(feature = "raft")]
    {
        let hosts = st.raft_hosts.as_ref().ok_or_else(raft_disabled)?;
        let host = hosts
            .host_for_shard(shard)
            .ok_or_else(|| shard_not_owned(shard))?;
        let node = crate::raft::NodeId::new(req.node_id);
        let index = crate::raft::add_learner(host, node, req.url)
            .await
            .map_err(refusal)?;
        Ok(Json(MembershipResponse {
            shard,
            node_id: req.node_id,
            index: index.get(),
        }))
    }
    #[cfg(not(feature = "raft"))]
    {
        let _ = (&st, shard, req);
        Err(raft_disabled())
    }
}

/// Promote the caught-up learner `node_id` of shard `shard` to voter.
#[utoipa::path(
    post,
    path = "/admin/raft/{shard}/members/{node_id}/promote",
    tag = "Admin",
    params(
        ("shard" = u32, Path, description = "Shard group id"),
        ("node_id" = u64, Path, description = "Raft node id of the learner")
    ),
    responses(
        (status = 200, description = "Learner promoted to voter", body = MembershipResponse),
        (status = 404, description = "Raft disabled, or unknown node", body = ApiError),
        (status = 409, description = "Shard not hosted here, or a change is refused", body = ApiError)
    )
)]
pub async fn promote_learner(
    State(st): State<AppState>,
    Path((shard, node_id)): Path<(u32, u64)>,
) -> Result<Json<MembershipResponse>, ApiErr> {
    #[cfg(feature = "raft")]
    {
        let hosts = st.raft_hosts.as_ref().ok_or_else(raft_disabled)?;
        let host = hosts
            .host_for_shard(shard)
            .ok_or_else(|| shard_not_owned(shard))?;
        let index = crate::raft::promote_learner(host, crate::raft::NodeId::new(node_id))
            .await
            .map_err(refusal)?;
        Ok(Json(MembershipResponse {
            shard,
            node_id,
            index: index.get(),
        }))
    }
    #[cfg(not(feature = "raft"))]
    {
        let _ = (&st, shard, node_id);
        Err(raft_disabled())
    }
}

/// Remove member `node_id` from shard `shard`'s group.
#[utoipa::path(
    delete,
    path = "/admin/raft/{shard}/members/{node_id}",
    tag = "Admin",
    params(
        ("shard" = u32, Path, description = "Shard group id"),
        ("node_id" = u64, Path, description = "Raft node id to remove")
    ),
    responses(
        (status = 200, description = "Member removed", body = MembershipResponse),
        (status = 404, description = "Raft disabled, or unknown node", body = ApiError),
        (status = 409, description = "Shard not hosted here, or a change is refused", body = ApiError)
    )
)]
pub async fn remove_member(
    State(st): State<AppState>,
    Path((shard, node_id)): Path<(u32, u64)>,
) -> Result<Json<MembershipResponse>, ApiErr> {
    #[cfg(feature = "raft")]
    {
        let hosts = st.raft_hosts.as_ref().ok_or_else(raft_disabled)?;
        let host = hosts
            .host_for_shard(shard)
            .ok_or_else(|| shard_not_owned(shard))?;
        let index = crate::raft::remove_member(host, crate::raft::NodeId::new(node_id))
            .await
            .map_err(refusal)?;
        Ok(Json(MembershipResponse {
            shard,
            node_id,
            index: index.get(),
        }))
    }
    #[cfg(not(feature = "raft"))]
    {
        let _ = (&st, shard, node_id);
        Err(raft_disabled())
    }
}
