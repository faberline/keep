//! HTTP error envelope and the `KvError` -> status-code mapping.
//!
//! Stays local: core `service_http::ApiErr` (v0.4.14) renders `{"error","message"}`
//! and takes a `&'static str` kind. Keep's public HTTP API renders `{"code","message"}`
//! with a dynamic code. Adopting core would rename a wire field (see the version
//! compatibility policy), so the local `ApiErr` stays (faberline/aw#292).

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::error::KvError;

/// Error envelope returned by every endpoint on failure.
#[derive(Debug, Serialize, ToSchema)]
pub struct ApiError {
    /// Stable machine-readable code, e.g. `key_not_found`, `cas_conflict`.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

/// An HTTP-typed error: a status code plus the JSON envelope.
#[derive(Debug)]
pub struct ApiErr {
    pub status: StatusCode,
    pub body: ApiError,
}

impl ApiErr {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiError {
                code: code.to_string(),
                message: message.into(),
            },
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn unsupported_media_type(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            message,
        )
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<KvError> for ApiErr {
    fn from(e: KvError) -> Self {
        let (status, code) = match &e {
            KvError::KeyNotFound(_) => (StatusCode::NOT_FOUND, "key_not_found"),
            KvError::KeyTooLong(_) | KvError::EmptyKey => (StatusCode::BAD_REQUEST, "invalid_key"),
            KvError::TypeMismatch { .. } => (StatusCode::CONFLICT, "type_mismatch"),
            KvError::CasConflict { .. } => (StatusCode::CONFLICT, "cas_conflict"),
            KvError::LockNotHeld => (StatusCode::CONFLICT, "lock_not_held"),
            KvError::LockOwnerMismatch { .. } => (StatusCode::CONFLICT, "lock_owner_mismatch"),
            KvError::OutOfMemory => (StatusCode::INSUFFICIENT_STORAGE, "out_of_memory"),
            KvError::IndexOutOfRange { .. } => (StatusCode::BAD_REQUEST, "index_out_of_range"),
            KvError::Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "storage_error"),
        };
        ApiErr::new(status, code, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails until the module doc records why the local `ApiErr` stays.
    #[test]
    fn module_doc_records_why_apierr_stays_local() {
        let src = include_str!("error.rs");
        assert!(
            src.lines()
                .any(|l| l.starts_with("//! Stays local: core `service_http::ApiErr` (v0.4.14)")),
            "module doc must record why the local ApiErr stays (faberline/aw#292)"
        );
    }

    /// The wire shape is `code` + `message`, never `error`.
    #[test]
    fn error_body_keeps_code_and_message() {
        let e = ApiErr::from(KvError::EmptyKey);
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        let v = serde_json::to_value(&e.body).expect("serialize");
        assert_eq!(v["code"], serde_json::json!("invalid_key"));
        assert!(v["message"].is_string());
        assert!(v.get("error").is_none());
    }
}
