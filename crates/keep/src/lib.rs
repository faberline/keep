//! keep — cloud-native, multi-core key-value / claim-check store.
//!
//! # Workspace crates
//! - [`keep_shared_kernel`] (`cluster`, `error`, `metrics`, `tls`, `types`) — foundational types, topology, and metrics
//! - [`keep_storage`] — the WAL, the binary file formats, and the blob store
//! - [`keep_keyspace`] (`engine`, `persistence`) — the sharded in-memory store, snapshots, and recovery
//! - [`keep_replication`] (`backup`, `raft`) — backup/restore and the Raft state machine
//! - [`keep_access`] — the bearer verifier over `service-auth`
//! - [`keep_commands`] (`http`, `spec`) — the HTTP/2 + OpenAPI interface (axum over hyper) and offline spec emitters
//! - [`keep_operator`] (`operator`) — Kubernetes CRD, manifest renderer, and reconcile controller
//! - [`keep_client`] (`client`) — HTTP/2 client for Rust consumers
//!
//! The engine and persistence layers are transport-agnostic; [`http`] is the
//! only network surface. Polyglot clients integrate against the generated
//! OpenAPI document at `/openapi.json`.

#![allow(clippy::all)]

pub use keep_access as access;
pub use keep_commands::{http, router, spec, ApiDoc, AppState};
pub use keep_keyspace::{engine, persistence, EvictionPolicy, KvEngine};
#[cfg(feature = "raft")]
pub use keep_replication::raft;
pub use keep_replication::{self as replication, backup};
pub use keep_shared_kernel::{
    cluster, error, metrics, tls, types, ClusterConfig, ClusterState, KvError, KvKey, KvValue,
};
pub use keep_storage as storage;

#[cfg(feature = "client")]
pub use keep_client as client;
#[cfg(feature = "operator")]
pub use keep_operator as operator;
