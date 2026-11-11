///! Snapshot creation and loading
///!
///! Provides periodic full engine state snapshots for faster recovery.
use super::{PersistenceError, Result, SnapshotConfig};
use crate::engine::{Entry, KvEngine};
use crate::types::KvValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use storage_durable::{FsyncPolicy, SnapshotFileStore};
use tracing::{debug, info};

/// Snapshot file magic: "KVSN0001"
pub const SNAPSHOT_MAGIC: &[u8; 8] = b"KVSN0001";
pub const SNAPSHOT_VERSION: u32 = 1;

/// Snapshot file header (64 bytes)
#[derive(Debug, Clone)]
pub struct SnapshotHeader {
    /// Magic number for validation
    pub magic: [u8; 8],
    /// Format version
    pub version: u32,
    /// When snapshot was created (nanoseconds since UNIX_EPOCH)
    pub created_at: i64,
    /// Number of shards in snapshot
    pub num_shards: u32,
    /// Total number of entries across all shards
    pub total_entries: u64,
    /// WAL position at time of snapshot (for recovery)
    pub wal_position: u64,
    /// SHA256 checksum of snapshot data
    pub checksum: [u8; 32],
}

impl SnapshotHeader {
    /// Create a new snapshot header
    pub fn new(num_shards: u32, total_entries: u64, wal_position: u64) -> Self {
        Self {
            magic: *SNAPSHOT_MAGIC,
            version: SNAPSHOT_VERSION,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
            num_shards,
            total_entries,
            wal_position,
            checksum: [0u8; 32], // Filled in after writing data
        }
    }

    /// Write header to file (64 bytes)
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.magic)?;
        writer.write_all(&self.version.to_be_bytes())?;
        writer.write_all(&self.created_at.to_be_bytes())?;
        writer.write_all(&self.num_shards.to_be_bytes())?;
        writer.write_all(&self.total_entries.to_be_bytes())?;
        writer.write_all(&self.wal_position.to_be_bytes())?;
        writer.write_all(&self.checksum)?;
        Ok(())
    }

    /// Read header from file
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;

        if magic != *SNAPSHOT_MAGIC {
            return Err(PersistenceError::CorruptedSnapshot(format!(
                "Invalid snapshot magic: expected {:?}, got {:?}",
                SNAPSHOT_MAGIC, magic
            )));
        }

        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf)?;
        let version = u32::from_be_bytes(buf);

        if version != SNAPSHOT_VERSION {
            return Err(PersistenceError::UnsupportedVersion(version));
        }

        let mut buf8 = [0u8; 8];

        reader.read_exact(&mut buf8)?;
        let created_at = i64::from_be_bytes(buf8);

        reader.read_exact(&mut buf)?;
        let num_shards = u32::from_be_bytes(buf);

        reader.read_exact(&mut buf8)?;
        let total_entries = u64::from_be_bytes(buf8);

        reader.read_exact(&mut buf8)?;
        let wal_position = u64::from_be_bytes(buf8);

        let mut checksum = [0u8; 32];
        reader.read_exact(&mut checksum)?;

        Ok(Self {
            magic,
            version,
            created_at,
            num_shards,
            total_entries,
            wal_position,
            checksum,
        })
    }
}

/// Serializable entry with absolute timestamps
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableEntry {
    pub value: KvValue,
    /// Nanoseconds since UNIX_EPOCH when entry was created
    pub created_at_nanos: i64,
    /// Nanoseconds since UNIX_EPOCH when entry expires (if any)
    pub expires_at_nanos: Option<i64>,
    pub version: u64,
}

impl SerializableEntry {
    /// Convert from runtime Entry to serializable format
    pub fn from_entry(entry: &Entry, base_instant: Instant) -> Self {
        let now_sys = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        let created_age = base_instant.saturating_duration_since(entry.created_at);
        let created_at_nanos = now_sys
            .checked_sub(created_age)
            .unwrap_or_default()
            .as_nanos() as i64;

        let expires_at_nanos = entry.expires_at.map(|exp| {
            if exp >= base_instant {
                let remaining = exp.saturating_duration_since(base_instant);
                now_sys.checked_add(remaining).unwrap_or(now_sys).as_nanos() as i64
            } else {
                let elapsed = base_instant.saturating_duration_since(exp);
                now_sys.checked_sub(elapsed).unwrap_or_default().as_nanos() as i64
            }
        });

        let value = entry
            .materialize_value()
            .unwrap_or_else(|| entry.value.clone());

        Self {
            value,
            created_at_nanos,
            expires_at_nanos,
            version: entry.version,
        }
    }

    /// Convert to runtime Entry
    pub fn to_entry(&self) -> Entry {
        let now = Instant::now();
        let now_absolute = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;

        let created_offset =
            Duration::from_nanos((now_absolute - self.created_at_nanos).max(0) as u64);
        let created_at = now.checked_sub(created_offset).unwrap_or(now);

        let expires_at = self.expires_at_nanos.map(|exp_nanos| {
            if exp_nanos > now_absolute {
                let remaining = Duration::from_nanos((exp_nanos - now_absolute) as u64);
                now.checked_add(remaining).unwrap_or(now)
            } else {
                let expired_ago = Duration::from_nanos((now_absolute - exp_nanos).max(1) as u64);
                now.checked_sub(expired_ago).unwrap_or(now)
            }
        });

        Entry::with_timestamps(self.value.clone(), created_at, expires_at, self.version)
    }
}

/// Serializable shard data
#[derive(Debug, Serialize, Deserialize)]
pub struct SerializableShard {
    pub shard_id: u32,
    pub entries: HashMap<String, SerializableEntry>,
}

/// Snapshot writer for creating snapshots
pub struct SnapshotWriter {
    config: SnapshotConfig,
}

impl SnapshotWriter {
    /// Create a new snapshot writer
    pub fn new(config: SnapshotConfig) -> Self {
        Self { config }
    }

    /// Create a snapshot of the engine
    pub fn create_snapshot(
        &self,
        engine: &KvEngine,
        data_dir: impl AsRef<Path>,
        wal_position: u64,
    ) -> Result<PathBuf> {
        let data_dir = data_dir.as_ref();

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        info!("Creating snapshot (WAL position: {})", wal_position);

        let mut hasher = Sha256::new();
        let mut data_buffer = Vec::new();

        // Serialize all shards
        let base_instant = Instant::now();
        let num_shards = engine.num_shards();
        let mut total_entries = 0u64;

        for shard_id in 0..num_shards {
            let shard_data = self.export_shard(engine, shard_id, base_instant)?;
            total_entries += shard_data.entries.len() as u64;

            let shard_bytes = bincode::serialize(&shard_data)?;
            data_buffer.extend_from_slice(&shard_bytes);
        }

        // Calculate checksum
        hasher.update(&data_buffer);
        let checksum: [u8; 32] = hasher.finalize().into();

        // Header + data, written atomically by the snapshot file store
        let mut header = SnapshotHeader::new(num_shards as u32, total_entries, wal_position);
        header.checksum = checksum;
        let mut bytes = Vec::with_capacity(72 + data_buffer.len());
        header.write(&mut bytes)?;
        bytes.extend_from_slice(&data_buffer);
        let final_path = snapshot_store(data_dir)?
            .save(timestamp as u64, &bytes)
            .map_err(store_err)?;

        info!(
            "Snapshot created at {}: {} entries across {} shards",
            final_path.display(),
            total_entries,
            num_shards
        );

        // Cleanup old snapshots and rotated WAL segments superseded by this snapshot
        self.cleanup_old_snapshots(data_dir)?;
        let _ = super::wal::cleanup_old_wal_files(data_dir, 0);

        Ok(final_path)
    }

    /// Export a single shard's data
    fn export_shard(
        &self,
        engine: &KvEngine,
        shard_id: usize,
        base_instant: Instant,
    ) -> Result<SerializableShard> {
        let entries = engine.export_shard(shard_id).ok_or_else(|| {
            PersistenceError::DataDirectory(format!("Invalid shard_id: {}", shard_id))
        })?;

        // Convert live (non-expired) entries to serializable format
        let serializable_entries = entries
            .into_iter()
            .filter(|(_, entry)| !entry.is_expired())
            .map(|(key, entry)| {
                let serializable_entry = SerializableEntry::from_entry(&entry, base_instant);
                (key, serializable_entry)
            })
            .collect();

        Ok(SerializableShard {
            shard_id: shard_id as u32,
            entries: serializable_entries,
        })
    }

    /// Cleanup old snapshots, keeping only the N most recent
    fn cleanup_old_snapshots(&self, data_dir: impl AsRef<Path>) -> Result<()> {
        let removed = snapshot_store(data_dir.as_ref())?
            .prune(self.config.keep_count)
            .map_err(store_err)?;
        debug!("Pruned {} old snapshot(s)", removed);
        Ok(())
    }
}

/// Snapshot loader for restoring from snapshots
pub struct SnapshotLoader;

impl SnapshotLoader {
    /// Load the latest snapshot from data directory
    pub fn load_latest(data_dir: impl AsRef<Path>) -> Result<Option<(SnapshotData, u64)>> {
        let data_dir = data_dir.as_ref();
        if !data_dir.exists() {
            // The store would create the directory; a missing one holds no snapshot
            return Ok(None);
        }

        let Some(bytes) = snapshot_store(data_dir)?.load_latest().map_err(store_err)? else {
            return Ok(None);
        };
        info!("Loading latest snapshot from {}", data_dir.display());

        // Read and validate header
        let mut reader = &bytes[..];
        let header = SnapshotHeader::read(&mut reader)?;

        // The rest is the shard data
        let data_buffer = reader;

        // Verify checksum
        let mut hasher = Sha256::new();
        hasher.update(&data_buffer);
        let computed_checksum: [u8; 32] = hasher.finalize().into();

        if computed_checksum != header.checksum {
            return Err(PersistenceError::ChecksumMismatch {
                pos: 0,
                expected: u32::from_be_bytes(header.checksum[0..4].try_into().unwrap()),
                actual: u32::from_be_bytes(computed_checksum[0..4].try_into().unwrap()),
            });
        }

        // Deserialize shards
        let mut shards = Vec::new();
        let mut offset = 0;

        for _ in 0..header.num_shards {
            let shard: SerializableShard = bincode::deserialize(&data_buffer[offset..])?;
            offset += bincode::serialized_size(&shard)? as usize;
            shards.push(shard);
        }

        info!(
            "Loaded snapshot: {} entries across {} shards",
            header.total_entries, header.num_shards
        );

        Ok(Some((
            SnapshotData {
                shards,
                total_entries: header.total_entries,
            },
            header.wal_position,
        )))
    }
}

/// Snapshot data loaded from file
#[derive(Debug)]
pub struct SnapshotData {
    pub shards: Vec<SerializableShard>,
    pub total_entries: u64,
}

/// Snapshot files in `data_dir`, named `snapshot-{millis}.snap`
fn snapshot_store(data_dir: &Path) -> Result<SnapshotFileStore> {
    SnapshotFileStore::new(data_dir, "snapshot", "snap", FsyncPolicy::Always).map_err(store_err)
}

fn store_err<E: std::fmt::Display>(err: E) -> PersistenceError {
    PersistenceError::DataDirectory(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::KvKey;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_snapshot_header_roundtrip() {
        let header = SnapshotHeader::new(256, 1000, 12345);

        let mut buffer = Vec::new();
        header.write(&mut buffer).unwrap();

        let mut cursor = &buffer[..];
        let decoded = SnapshotHeader::read(&mut cursor).unwrap();

        assert_eq!(decoded.magic, *SNAPSHOT_MAGIC);
        assert_eq!(decoded.version, SNAPSHOT_VERSION);
        assert_eq!(decoded.num_shards, 256);
        assert_eq!(decoded.total_entries, 1000);
        assert_eq!(decoded.wal_position, 12345);
    }

    #[test]
    fn test_serializable_entry_roundtrip() {
        use crate::types::KvValue;

        let base = Instant::now();
        let original = Entry::with_timestamps(
            KvValue::String("test".to_string()),
            base,
            Some(base + Duration::from_secs(60)),
            42,
        );

        let serializable = SerializableEntry::from_entry(&original, base);
        let restored = serializable.to_entry();

        // Values should match
        assert_eq!(restored.value, original.value);
        assert_eq!(restored.version, original.version);

        // Restored entry must not be expired and must retain ~60s TTL
        assert!(restored.created_at <= Instant::now());
        assert!(
            !restored.is_expired(),
            "Restored entry with future TTL must not be expired"
        );
        let remaining = restored
            .expires_at
            .expect("expires_at should be present")
            .saturating_duration_since(Instant::now());
        assert!(
            remaining >= Duration::from_secs(55) && remaining <= Duration::from_secs(61),
            "Expected ~60s remaining TTL, got {:?}",
            remaining
        );
    }

    /// Write a one-shard snapshot holding `keys` live keys, covering WAL `wal_seq`.
    fn write_snapshot(dir: &Path, keys: usize, wal_seq: u64) -> PathBuf {
        let engine = KvEngine::with_shards(4);
        for i in 0..keys {
            engine
                .set(
                    &KvKey::new(format!("k{i}")).unwrap(),
                    KvValue::Int(i as i64),
                    None,
                )
                .unwrap();
        }
        SnapshotWriter::new(SnapshotConfig::default())
            .create_snapshot(&engine, dir, wal_seq)
            .unwrap()
    }

    #[test]
    fn test_load_latest_orders_by_number_not_text() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        // By text, "snapshot-999" sorts after "snapshot-1000"; by number it is older.
        let first = write_snapshot(data_dir, 1, 7);
        fs::copy(&first, data_dir.join("snapshot-999.snap")).unwrap();
        fs::remove_file(&first).unwrap();
        let second = write_snapshot(data_dir, 2, 9);
        fs::copy(&second, data_dir.join("snapshot-1000.snap")).unwrap();
        fs::remove_file(&second).unwrap();

        let (data, wal_seq) = SnapshotLoader::load_latest(data_dir).unwrap().unwrap();
        assert_eq!(wal_seq, 9);
        assert_eq!(data.total_entries, 2);
    }

    #[test]
    fn test_snapshot_file_from_current_build_loads() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        // Bytes in the current format: 72-byte header, then one bincode shard.
        let base = Instant::now();
        let entry = Entry::with_timestamps(KvValue::Int(7), base, None, 1);
        let mut entries = HashMap::new();
        entries.insert("k".to_string(), SerializableEntry::from_entry(&entry, base));
        let data = bincode::serialize(&SerializableShard {
            shard_id: 0,
            entries,
        })
        .unwrap();

        let mut hasher = Sha256::new();
        hasher.update(&data);
        let mut header = SnapshotHeader::new(1, 1, 5);
        header.checksum = hasher.finalize().into();

        let mut bytes = Vec::new();
        header.write(&mut bytes).unwrap();
        bytes.extend_from_slice(&data);
        fs::write(data_dir.join("snapshot-1000.snap"), &bytes).unwrap();

        let (loaded, wal_seq) = SnapshotLoader::load_latest(data_dir).unwrap().unwrap();
        assert_eq!(wal_seq, 5);
        assert_eq!(loaded.total_entries, 1);
        assert_eq!(loaded.shards.len(), 1);
    }

    #[test]
    fn test_prune_keeps_newest_keep_count() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();
        let engine = KvEngine::with_shards(4);
        let writer = SnapshotWriter::new(SnapshotConfig {
            keep_count: 2,
            ..Default::default()
        });

        for seq in 1..=4 {
            writer.create_snapshot(&engine, data_dir, seq).unwrap();
            // Snapshot names are millisecond timestamps; keep them distinct.
            std::thread::sleep(Duration::from_millis(2));
        }

        let remaining = fs::read_dir(data_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("snapshot-") && n.ends_with(".snap"))
            .count();
        assert_eq!(remaining, 2);

        let (_, latest_seq) = SnapshotLoader::load_latest(data_dir).unwrap().unwrap();
        assert_eq!(latest_seq, 4);
    }

    #[test]
    fn test_load_latest_missing_dir_returns_none_and_creates_nothing() {
        let temp_dir = TempDir::new().unwrap();
        let absent = temp_dir.path().join("absent");

        assert!(SnapshotLoader::load_latest(&absent).unwrap().is_none());
        assert!(!absent.exists());
    }

    #[test]
    fn test_snapshot_header_invalid_magic() {
        let mut buffer = vec![0u8; 64];
        buffer[0..8].copy_from_slice(b"INVALID!");

        let result = SnapshotHeader::read(&mut &buffer[..]);
        assert!(result.is_err());
    }
}
