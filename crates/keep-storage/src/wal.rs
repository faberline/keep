//! Write-Ahead Log (WAL) on the core `storage-durable` framed log
//!
//! Provides append-only logging of all write operations with batched fsync.
//!
//! ## Architecture
//!
//! - Each entry is one frame of the core framed log:
//!   `[seq: u64 LE | len: u32 LE | crc32: u32 LE | payload]`, where the payload
//!   is `bincode(WalOp)`. `seq` is the entry's position in the WAL.
//! - A single WAL file grows until the rotation threshold, then rotates
//! - Background thread flushes periodically (batched fsync)
//! - The core writer drops a torn tail when a file is opened; a bad frame ends
//!   the log on read
//! - A file in the retired `KVWAL001` format is refused: never truncated, never read
//!
//! ## File Naming
//!
//! - Active WAL: `wal-current.log`
//! - Rotated WAL: `wal-{timestamp}.log`

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use storage_durable::{FramedLogCursor, FramedLogReader, FramedLogWriter, FsyncPolicy};
use tracing::{debug, info, warn};

use crate::format::{WalEntry, WalOp, WAL_MAGIC};
use crate::{PersistenceError, Result, WalConfig};

/// Name of the active WAL file.
const CURRENT_FILE: &str = "wal-current.log";

/// Bytes a frame adds around its payload: seq (8) + len (4) + crc32 (4).
const FRAME_OVERHEAD: u64 = 16;

fn wal_err<E: std::fmt::Display>(err: E) -> PersistenceError {
    PersistenceError::Wal(format!("{err:#}"))
}

/// Refuse a file in the retired `KVWAL001` format. Reads only its first 8 bytes.
fn refuse_legacy(path: &Path) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut magic = Vec::with_capacity(8);
    file.take(8).read_to_end(&mut magic)?;
    if magic.as_slice() == WAL_MAGIC {
        return Err(PersistenceError::LegacyWalFormat {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

/// WAL writer with batched fsync
pub struct WalWriter {
    log: FramedLogWriter,
    path: PathBuf,
    data_dir: PathBuf,
    /// Sequence number of the next entry.
    next_seq: u64,
    /// Bytes in the active file; drives the rotation threshold.
    bytes: u64,
    unflushed: bool,
    last_fsync: Instant,
    config: WalConfig,
}

impl WalWriter {
    /// Create a new WAL writer. The sequence continues after the highest `seq`
    /// found in the existing WAL files.
    pub fn new(data_dir: PathBuf, config: WalConfig) -> Result<Self> {
        // Ensure data directory exists
        fs::create_dir_all(&data_dir).map_err(|e| {
            PersistenceError::DataDirectory(format!("Failed to create directory: {}", e))
        })?;

        let mut max_seq = 0u64;
        for file in find_wal_files(&data_dir)? {
            refuse_legacy(&file)?;
            let last = FramedLogReader::visit_frames(&file, 0, |_| Ok(())).map_err(wal_err)?;
            max_seq = max_seq.max(last);
        }

        // Refuse before `open`: `open` truncates a bad tail, which would drop a legacy file.
        let path = data_dir.join(CURRENT_FILE);
        refuse_legacy(&path)?;
        let log = FramedLogWriter::open(&path, FsyncPolicy::Os).map_err(wal_err)?;
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

        Ok(Self {
            log,
            path,
            data_dir,
            next_seq: max_seq + 1,
            bytes,
            unflushed: false,
            last_fsync: Instant::now(),
            config,
        })
    }

    /// Append an operation to the WAL and return its sequence number
    pub fn append(&mut self, op: WalOp) -> Result<u64> {
        let payload = bincode::serialize(&op)?;
        let seq = self.next_seq;
        self.log.append(seq, &payload).map_err(wal_err)?;
        self.next_seq += 1;
        self.bytes += FRAME_OVERHEAD + payload.len() as u64;
        self.unflushed = true;
        Ok(seq)
    }

    /// Flush pending writes to disk (fsync)
    pub fn flush(&mut self) -> Result<()> {
        if !self.unflushed {
            return Ok(());
        }

        self.log.sync().map_err(wal_err)?;
        self.unflushed = false;
        self.last_fsync = Instant::now();

        debug!("WAL fsynced through seq {}", self.last_seq());

        Ok(())
    }

    /// Check if flush is needed based on time
    pub fn should_flush(&self) -> bool {
        self.last_fsync.elapsed().as_millis() >= self.config.flush_interval_ms as u128
    }

    /// Check if rotation is needed based on file size
    pub fn should_rotate(&self) -> bool {
        self.bytes >= self.config.max_file_size
    }

    /// Rotate to a new WAL file
    pub fn rotate(&mut self) -> Result<PathBuf> {
        // Flush pending writes before the file is renamed
        self.flush()?;

        // Get millisecond timestamp for rotated file (with monotonic suffix if same ms)
        static ROTATE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let seq = ROTATE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let rotated_path =
            self.data_dir
                .join(format!("wal-{:015}-{:04}.log", timestamp_ms, seq % 10000));

        fs::rename(&self.path, &rotated_path)?;
        // Opening creates the new, empty active file
        self.log = FramedLogWriter::open(&self.path, FsyncPolicy::Os).map_err(wal_err)?;

        info!(
            "Rotated WAL: {} -> {}",
            self.path.display(),
            rotated_path.display()
        );

        self.bytes = 0;
        self.unflushed = false;
        self.last_fsync = Instant::now();

        Ok(rotated_path)
    }

    /// Sequence number of the last entry written; 0 when none was ever written
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Make sure the next entry gets a sequence number after `seq`.
    /// Recovery calls this with the snapshot's `seq`, so no number is reused
    /// after the rotated files that held it were deleted.
    pub fn ensure_next_seq_after(&mut self, seq: u64) {
        self.next_seq = self.next_seq.max(seq + 1);
    }

    /// Get the WAL file path
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// WAL reader for replaying entries
pub struct WalReader {
    cursor: FramedLogCursor,
    path: PathBuf,
}

impl WalReader {
    /// Open a WAL file for reading. A file in the retired `KVWAL001` format is refused.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        refuse_legacy(&path)?;
        let cursor = FramedLogCursor::open(&path).map_err(wal_err)?;

        debug!("Opened WAL for reading: {}", path.display());

        Ok(Self { cursor, path })
    }

    /// Read the next entry. Returns `None` at the end of the log and at the
    /// first bad frame (torn tail or CRC mismatch).
    pub fn read_entry(&mut self) -> Result<Option<WalEntry>> {
        let Some(frame) = self.cursor.next_frame().map_err(wal_err)? else {
            return Ok(None);
        };

        let op: WalOp =
            bincode::deserialize(&frame.payload).map_err(|e| PersistenceError::CorruptedWal {
                pos: frame.seq,
                reason: format!("Deserialization failed: {}", e),
            })?;

        Ok(Some(WalEntry { seq: frame.seq, op }))
    }

    /// Get the WAL file path
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Find all WAL files in a directory
pub fn find_wal_files(data_dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    let data_dir = data_dir.as_ref();

    if !data_dir.exists() {
        return Ok(Vec::new());
    }

    let mut wal_files = Vec::new();

    for entry in fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();

        if let Some(filename) = path.file_name() {
            if let Some(name) = filename.to_str() {
                if name.starts_with("wal-") && name.ends_with(".log") {
                    wal_files.push(path);
                }
            }
        }
    }

    // Sort by filename (timestamp order)
    wal_files.sort();

    Ok(wal_files)
}

/// Delete old WAL files (keep only the most recent N)
pub fn cleanup_old_wal_files(data_dir: impl AsRef<Path>, keep_count: usize) -> Result<usize> {
    let mut wal_files = find_wal_files(data_dir)?;

    // Don't delete wal-current.log
    wal_files.retain(|p| {
        !p.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s == CURRENT_FILE)
            .unwrap_or(false)
    });

    if wal_files.len() <= keep_count {
        return Ok(0);
    }

    let to_delete = wal_files.len() - keep_count;
    let mut deleted = 0;

    for path in wal_files.iter().take(to_delete) {
        match fs::remove_file(path) {
            Ok(_) => {
                debug!("Deleted old WAL file: {}", path.display());
                deleted += 1;
            }
            Err(e) => {
                warn!("Failed to delete WAL file {}: {}", path.display(), e);
            }
        }
    }

    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keep_shared_kernel::types::KvValue;
    use std::time::Duration;
    use tempfile::TempDir;

    fn create_test_config() -> (TempDir, WalConfig) {
        let temp_dir = TempDir::new().unwrap();
        let config = WalConfig {
            flush_interval_ms: 100,
            max_file_size: 1024, // 1KB for testing rotation
        };
        (temp_dir, config)
    }

    #[test]
    fn test_wal_write_and_read() {
        let (temp_dir, config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();

        // Write operations
        let mut writer = WalWriter::new(data_dir.clone(), config).unwrap();

        let ops = vec![
            WalOp::Set {
                key: "key1".to_string(),
                value: KvValue::String("value1".to_string()),
                ttl: None,
            },
            WalOp::Set {
                key: "key2".to_string(),
                value: KvValue::Int(42),
                ttl: Some(Duration::from_secs(60)),
            },
            WalOp::Delete {
                key: "key1".to_string(),
            },
        ];

        for op in &ops {
            writer.append(op.clone()).unwrap();
        }
        writer.flush().unwrap();

        // Read operations
        let wal_path = data_dir.join("wal-current.log");
        let mut reader = WalReader::new(&wal_path).unwrap();

        let mut read_ops = Vec::new();
        while let Some(entry) = reader.read_entry().unwrap() {
            read_ops.push(entry.op);
        }

        assert_eq!(read_ops.len(), ops.len());

        // Verify operations match
        for (original, read) in ops.iter().zip(read_ops.iter()) {
            match (original, read) {
                (
                    WalOp::Set {
                        key: k1,
                        value: v1,
                        ttl: t1,
                    },
                    WalOp::Set {
                        key: k2,
                        value: v2,
                        ttl: t2,
                    },
                ) => {
                    assert_eq!(k1, k2);
                    assert_eq!(v1, v2);
                    assert_eq!(t1, t2);
                }
                (WalOp::Delete { key: k1 }, WalOp::Delete { key: k2 }) => {
                    assert_eq!(k1, k2);
                }
                _ => panic!("Op type mismatch"),
            }
        }
    }

    #[test]
    fn test_wal_rotation() {
        let (temp_dir, mut config) = create_test_config();
        config.max_file_size = 256; // Small size to trigger rotation
        let data_dir = temp_dir.path().to_path_buf();

        let mut writer = WalWriter::new(data_dir.clone(), config).unwrap();

        // Write enough data to trigger rotation
        for i in 0..20 {
            writer
                .append(WalOp::Set {
                    key: format!("key{}", i),
                    value: KvValue::String("x".repeat(50)),
                    ttl: None,
                })
                .unwrap();

            if writer.should_rotate() {
                writer.rotate().unwrap();
            }
        }

        // Should have multiple WAL files
        let wal_files = find_wal_files(&data_dir).unwrap();
        assert!(
            wal_files.len() > 1,
            "Expected multiple WAL files after rotation"
        );
    }

    #[test]
    fn test_wal_find_files() {
        let (temp_dir, config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();

        // Create some WAL files
        let mut writer = WalWriter::new(data_dir.clone(), config).unwrap();
        writer
            .append(WalOp::Delete {
                key: "test".to_string(),
            })
            .unwrap();
        writer.flush().unwrap();

        let files = find_wal_files(&data_dir).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].to_str().unwrap().contains("wal-current.log"));
    }

    #[test]
    fn test_wal_cleanup() {
        let (temp_dir, _config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();

        // Create multiple rotated WAL files
        for i in 0..5 {
            let path = data_dir.join(format!("wal-{}.log", 1000 + i));
            File::create(&path).unwrap();
        }

        let before = find_wal_files(&data_dir).unwrap();
        assert_eq!(before.len(), 5);

        // Keep only 2 files
        let deleted = cleanup_old_wal_files(&data_dir, 2).unwrap();
        assert_eq!(deleted, 3);

        let after = find_wal_files(&data_dir).unwrap();
        assert_eq!(after.len(), 2);
    }

    #[test]
    fn test_wal_batch_operations() {
        let (temp_dir, config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();

        let mut writer = WalWriter::new(data_dir.clone(), config).unwrap();

        // Write MSET
        writer
            .append(WalOp::MSet {
                pairs: vec![
                    ("k1".to_string(), KvValue::Int(1)),
                    ("k2".to_string(), KvValue::Int(2)),
                ],
                ttl: None,
            })
            .unwrap();

        // Write MDEL
        writer
            .append(WalOp::MDel {
                keys: vec!["k1".to_string(), "k2".to_string()],
            })
            .unwrap();

        writer.flush().unwrap();

        // Read back
        let mut reader = WalReader::new(data_dir.join("wal-current.log")).unwrap();

        let entry1 = reader.read_entry().unwrap().unwrap();
        assert_eq!(entry1.seq, 1);
        match entry1.op {
            WalOp::MSet { pairs, .. } => assert_eq!(pairs.len(), 2),
            _ => panic!("Expected MSet"),
        }

        let entry2 = reader.read_entry().unwrap().unwrap();
        assert_eq!(entry2.seq, 2);
        match entry2.op {
            WalOp::MDel { keys } => assert_eq!(keys.len(), 2),
            _ => panic!("Expected MDel"),
        }
    }

    #[test]
    fn test_wal_seq_is_monotonic_across_reopen() {
        let (temp_dir, config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();

        let mut writer = WalWriter::new(data_dir.clone(), config.clone()).unwrap();
        writer
            .append(WalOp::Delete {
                key: "a".to_string(),
            })
            .unwrap();
        writer
            .append(WalOp::Delete {
                key: "b".to_string(),
            })
            .unwrap();
        writer.flush().unwrap();
        drop(writer);

        let mut writer = WalWriter::new(data_dir.clone(), config).unwrap();
        assert_eq!(
            writer
                .append(WalOp::Delete {
                    key: "c".to_string()
                })
                .unwrap(),
            3
        );
        assert_eq!(writer.last_seq(), 3);
        writer.flush().unwrap();
        drop(writer);

        let mut reader = WalReader::new(data_dir.join("wal-current.log")).unwrap();
        let mut seqs = Vec::new();
        while let Some(entry) = reader.read_entry().unwrap() {
            seqs.push(entry.seq);
        }
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn test_wal_seq_continues_across_rotation() {
        let (temp_dir, config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();

        let mut writer = WalWriter::new(data_dir.clone(), config).unwrap();
        writer
            .append(WalOp::Delete {
                key: "a".to_string(),
            })
            .unwrap();
        writer
            .append(WalOp::Delete {
                key: "b".to_string(),
            })
            .unwrap();
        let rotated = writer.rotate().unwrap();
        assert_eq!(
            writer
                .append(WalOp::Delete {
                    key: "c".to_string()
                })
                .unwrap(),
            3
        );
        writer.flush().unwrap();
        drop(writer);

        let mut reader = WalReader::new(&rotated).unwrap();
        let mut seqs = Vec::new();
        while let Some(entry) = reader.read_entry().unwrap() {
            seqs.push(entry.seq);
        }
        assert_eq!(seqs, vec![1, 2]);

        let mut reader = WalReader::new(data_dir.join("wal-current.log")).unwrap();
        assert_eq!(reader.read_entry().unwrap().unwrap().seq, 3);
        assert!(reader.read_entry().unwrap().is_none());
    }

    #[test]
    fn test_wal_corrupted_frame_ends_the_log() {
        let (temp_dir, config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();

        let mut writer = WalWriter::new(data_dir.clone(), config).unwrap();
        for key in ["a", "b", "c"] {
            writer
                .append(WalOp::Set {
                    key: key.to_string(),
                    value: KvValue::String("value".to_string()),
                    ttl: None,
                })
                .unwrap();
        }
        writer.flush().unwrap();
        drop(writer);

        // Flip one payload byte of the last frame.
        let wal_path = data_dir.join("wal-current.log");
        let mut bytes = fs::read(&wal_path).unwrap();
        let idx = bytes.len() - 3;
        bytes[idx] ^= 0xFF;
        fs::write(&wal_path, &bytes).unwrap();

        let mut reader = WalReader::new(&wal_path).unwrap();
        let first = reader.read_entry().unwrap().unwrap();
        let second = reader.read_entry().unwrap().unwrap();
        assert_eq!((first.seq, second.seq), (1, 2));
        assert!(reader.read_entry().unwrap().is_none());
    }

    #[test]
    fn test_wal_refuses_legacy_file() {
        let (temp_dir, config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();
        let wal_path = data_dir.join("wal-current.log");

        let mut legacy = b"KVWAL001".to_vec();
        legacy.extend_from_slice(&[0u8; 24]);
        fs::write(&wal_path, &legacy).unwrap();

        assert!(matches!(
            WalWriter::new(data_dir.clone(), config),
            Err(PersistenceError::LegacyWalFormat { .. })
        ));
        assert!(matches!(
            WalReader::new(&wal_path),
            Err(PersistenceError::LegacyWalFormat { .. })
        ));
        // Not truncated, not read as data.
        assert_eq!(fs::read(&wal_path).unwrap(), legacy);
    }

    #[test]
    fn test_wal_ensure_next_seq_after() {
        let (temp_dir, config) = create_test_config();
        let mut writer = WalWriter::new(temp_dir.path().to_path_buf(), config).unwrap();

        writer.ensure_next_seq_after(41);
        assert_eq!(
            writer
                .append(WalOp::Delete {
                    key: "x".to_string()
                })
                .unwrap(),
            42
        );

        writer.ensure_next_seq_after(3);
        assert_eq!(
            writer
                .append(WalOp::Delete {
                    key: "y".to_string()
                })
                .unwrap(),
            43
        );
    }

    #[test]
    fn test_wal_flush_timing() {
        let (temp_dir, config) = create_test_config();
        let data_dir = temp_dir.path().to_path_buf();

        let mut writer = WalWriter::new(data_dir, config).unwrap();

        // Initially should not need flush
        assert!(!writer.should_flush());

        // Write an operation
        writer
            .append(WalOp::Set {
                key: "test".to_string(),
                value: KvValue::String("value".to_string()),
                ttl: None,
            })
            .unwrap();

        // Still shouldn't need flush immediately
        assert!(!writer.should_flush());

        // Wait for flush interval
        std::thread::sleep(std::time::Duration::from_millis(150));

        // Now should need flush
        assert!(writer.should_flush());

        writer.flush().unwrap();
        assert!(!writer.should_flush());
    }
}
