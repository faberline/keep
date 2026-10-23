//! The keyspace: keep's sharded multi-core KV engine, plus the engine-aware
//! snapshot, recovery, and persistence handle.

#![allow(clippy::all)]

pub use keep_shared_kernel::{
    cluster, error, metrics, tls, types, Cluster, ClusterConfig, ClusterState, KvError, KvKey,
    KvValue,
};

pub mod apply;
pub mod engine;
pub mod persistence;

pub use engine::{EvictionPolicy, KvEngine};
