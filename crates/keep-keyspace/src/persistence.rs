//! Engine-aware persistence: the snapshot writer and loader, crash recovery,
//! and the handle the engine logs through. The on-disk primitives (WAL,
//! formats, blob store, settings, errors) come from `keep-storage` and are
//! re-exported here so `keep::persistence::*` paths stay the same.

pub use keep_storage::*;

pub mod handle;
pub mod recovery;
pub mod snapshot;

pub use handle::PersistenceHandle;
