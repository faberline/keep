//! keep — cloud-native, multi-core key-value / claim-check store.
//!
//! # Workspace Crates & Layers
//! - [`keep_core`] (`cluster`, `error`, `metrics`, `tls`, `types`) — foundational types, topology, and metrics
//! - [`keep_engine`] (`engine`, `persistence`, `backup`, `raft`) — sharded in-memory store, WAL/snapshot durability, backup, and Raft state machine
//! - [`keep_http`] (`http`, `spec`) — HTTP/2 + OpenAPI transport (axum over hyper) and offline spec emitters
//! - [`keep_operator`] (`operator`) — Kubernetes CRD, manifest renderer, and reconcile controller
//! - [`keep_client`] (`client`) — HTTP/2 client for Rust consumers
//!
//! The engine and persistence layers are transport-agnostic; [`http`] is the
//! only network surface. Polyglot clients integrate against the generated
//! OpenAPI document at `/openapi.json`.

#![allow(clippy::all)]

pub use keep_core::{
    cluster, error, metrics, tls, types, ClusterConfig, ClusterState, KvError, KvKey, KvValue,
    ReplicaPlacement,
};
#[cfg(feature = "raft")]
pub use keep_engine::raft;
pub use keep_engine::{apply, backup, engine, persistence, EvictionPolicy, KvEngine};
pub use keep_http::{http, router, spec, ApiDoc, AppState};

#[cfg(feature = "client")]
pub use keep_client as client;
#[cfg(feature = "operator")]
pub use keep_operator as operator;
