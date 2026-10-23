//! keep's commands: the HTTP/2 + OpenAPI interface over the keyspace, and the
//! offline spec emitters.

#![allow(clippy::all)]

pub use keep_keyspace::{engine, persistence, EvictionPolicy, KvEngine};
pub use keep_replication::backup;
#[cfg(feature = "raft")]
pub use keep_replication::raft;
pub use keep_shared_kernel::{
    cluster, error, metrics, tls, types, Cluster, ClusterConfig, ClusterState, KvError, KvKey,
    KvValue,
};

pub mod http;
pub mod spec;

pub use http::{router, ApiDoc, AppState, DEFAULT_BODY_LIMIT};
