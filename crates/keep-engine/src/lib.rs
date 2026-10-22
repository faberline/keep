//! Sharded multi-core KV engine, WAL/snapshot persistence, backup, and Raft state machine for `keep`.

#![allow(clippy::all)]

pub use keep_core::{
    cluster, error, metrics, tls, types, Cluster, ClusterConfig, ClusterState, KvError, KvKey,
    KvValue,
};

pub mod apply;
pub mod backup;
pub mod engine;
pub mod persistence;
#[cfg(feature = "raft")]
pub mod raft;
#[cfg(feature = "raft")]
pub mod raft_read;

pub use engine::{EvictionPolicy, KvEngine};
