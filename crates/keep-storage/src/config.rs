//! Persistence settings: the data directory, WAL, and snapshot options.

use std::path::PathBuf;
use std::time::Duration;

/// Persistence configuration
#[derive(Debug, Clone)]
pub struct PersistenceConfig {
    /// Data directory path (default: ./data)
    pub data_dir: PathBuf,

    /// WAL configuration
    pub wal_config: WalConfig,

    /// Snapshot configuration
    pub snapshot_config: SnapshotConfig,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            wal_config: WalConfig::default(),
            snapshot_config: SnapshotConfig::default(),
        }
    }
}

impl PersistenceConfig {
    /// Create a new persistence config with the specified data directory
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            wal_config: WalConfig::default(),
            snapshot_config: SnapshotConfig::default(),
        }
    }

    /// Set WAL fsync interval in milliseconds
    pub fn with_fsync_interval_ms(mut self, ms: u64) -> Self {
        self.wal_config.flush_interval_ms = ms;
        self
    }

    /// Set snapshot interval in seconds
    pub fn with_snapshot_interval_secs(mut self, secs: u64) -> Self {
        self.snapshot_config.interval_secs = secs;
        self
    }

    /// Set snapshot operations threshold
    pub fn with_snapshot_ops_threshold(mut self, ops: usize) -> Self {
        self.snapshot_config.ops_threshold = ops;
        self
    }
}

/// WAL (Write-Ahead Log) configuration
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Flush interval in milliseconds (default: 100ms)
    pub flush_interval_ms: u64,

    /// Maximum WAL file size before rotation (default: 1GB)
    pub max_file_size: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            flush_interval_ms: 100,
            max_file_size: 1024 * 1024 * 1024, // 1GB
        }
    }
}

/// Snapshot configuration
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    /// Snapshot interval in seconds (default: 5 minutes)
    pub interval_secs: u64,

    /// Operations threshold for snapshot (default: 100K operations)
    pub ops_threshold: usize,

    /// Number of snapshots to keep (default: 3)
    pub keep_count: usize,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            interval_secs: 300,     // 5 minutes
            ops_threshold: 100_000, // 100K operations
            keep_count: 3,
        }
    }
}

/// Recovery statistics
#[derive(Debug, Default, Clone)]
pub struct RecoveryStats {
    /// Whether a snapshot was loaded
    pub snapshot_loaded: bool,

    /// Number of entries loaded from snapshot
    pub snapshot_entries: usize,

    /// Number of WAL entries replayed
    pub wal_entries_replayed: usize,

    /// Number of corrupted entries skipped
    pub corrupted_entries: usize,

    /// Total recovery duration
    pub recovery_duration: Duration,
}
