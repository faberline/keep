//! keep's replication: the Raft state machine over `raft-runtime` and the
//! backup/restore runner over `service-backup`.

#![allow(clippy::all)]

pub mod backup;
#[cfg(feature = "raft")]
pub mod raft;
