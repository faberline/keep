//! keep's on-disk primitives: the WAL, the binary file formats, and the
//! blob store for spilled values. The engine-aware snapshot, recovery, and
//! persistence handle live in `keep-keyspace`.
//!
//! Provides crash-safe durability using Write-Ahead Log (WAL) with periodic snapshots.
//!
//! ## Architecture
//!
//! - **WAL**: Append-only log recording all write operations
//! - **Snapshots**: Periodic full engine state snapshots for faster recovery
//! - **Batched Fsync**: 100ms flush intervals for optimal throughput
//! - **Recovery**: Load snapshot + replay WAL delta
//!
//! ## Durability Guarantee
//!
//! All writes are logged to WAL before acknowledging to client. With 100ms batched
//! fsync, the system can lose at most ~100ms of data on crash (acceptable for task backends).

pub mod blob_store;
pub mod config;
pub mod error;
pub mod format;
pub mod wal;

pub use blob_store::{BlobStore, DiskBlob};
pub use config::{PersistenceConfig, RecoveryStats, SnapshotConfig, WalConfig};
pub use error::{PersistenceError, Result};
