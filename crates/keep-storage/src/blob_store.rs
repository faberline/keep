//! Tiered disk blob storage (`/data/blobs/`) for large or cold values.
//!
//! Keeps hot metadata and small values in RAM while spilling large `Bytes`/`String`
//! payloads (or cold values under memory pressure) to content-verified files on
//! the persistent volume. When an `Entry` is overwritten, deleted, expired, or
//! evicted, dropping its `Arc<DiskBlob>` automatically unlinks the file and
//! reclaims disk quota.

use crate::{PersistenceError, Result};
use keep_shared_kernel::types::KvValue;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};

/// Reference-counted handle to a value payload stored on disk.
///
/// Cloning an `Entry` clones `Arc<DiskBlob>` cheaply without copying the file.
/// When the last reference is dropped, the underlying file is deleted and
/// `used_disk_bytes` is decremented.
#[derive(Debug)]
pub struct DiskBlob {
    /// Unique monotonic blob ID
    pub id: u64,
    /// Path to the blob file on disk
    pub path: PathBuf,
    /// Payload size in bytes
    pub len: usize,
    /// True if the original value was `KvValue::String`, false for `KvValue::Bytes`
    pub is_string: bool,
    /// CRC32 checksum of the raw payload bytes
    pub crc32: u32,
    /// Shared counter tracking total bytes stored in the blob tier
    used_disk_bytes: Arc<AtomicUsize>,
}

impl DiskBlob {
    /// Read and verify the blob payload from disk, reconstructing the original [`KvValue`].
    pub fn read_value(&self) -> Result<KvValue> {
        let mut file = File::open(&self.path)?;
        let mut buf = Vec::with_capacity(self.len);
        file.read_to_end(&mut buf)?;

        let actual_crc = crc32fast::hash(&buf);
        if actual_crc != self.crc32 {
            return Err(PersistenceError::ChecksumMismatch {
                pos: 0,
                expected: self.crc32,
                actual: actual_crc,
            });
        }

        if self.is_string {
            let s = String::from_utf8(buf).map_err(|e| {
                PersistenceError::CorruptedSnapshot(format!("Invalid UTF-8 in disk blob: {}", e))
            })?;
            Ok(KvValue::String(s))
        } else {
            Ok(KvValue::Bytes(buf))
        }
    }
}

impl Drop for DiskBlob {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to remove disk blob {}: {}", self.path.display(), e);
            }
        }
        let len = self.len;
        let _ = self
            .used_disk_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(len))
            });
    }
}

/// Manages disk-tiered blob files under `<data_dir>/blobs/`.
#[derive(Debug)]
pub struct BlobStore {
    blob_dir: PathBuf,
    next_id: AtomicU64,
    used_disk_bytes: Arc<AtomicUsize>,
    /// Payloads >= this size (in bytes) are spilled directly to disk on write (0 = disabled).
    spill_threshold_bytes: AtomicUsize,
    /// Maximum total bytes allowed in the blob directory (0 = unlimited).
    max_disk_bytes: AtomicUsize,
}

impl BlobStore {
    /// Initialize a blob store at `<data_dir>/blobs`, cleaning up any orphaned
    /// blob files from prior runs (ground truth is restored from Snapshot + WAL).
    pub fn new(
        data_dir: impl AsRef<Path>,
        spill_threshold_bytes: usize,
        max_disk_bytes: usize,
    ) -> Result<Self> {
        let blob_dir = data_dir.as_ref().join("blobs");
        fs::create_dir_all(&blob_dir).map_err(|e| {
            PersistenceError::DataDirectory(format!(
                "Failed to create blob directory {}: {}",
                blob_dir.display(),
                e
            ))
        })?;

        // Clean up any stale blob files left over before recovery
        if let Ok(entries) = fs::read_dir(&blob_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let _ = fs::remove_file(path);
                }
            }
        }

        Ok(Self {
            blob_dir,
            next_id: AtomicU64::new(1),
            used_disk_bytes: Arc::new(AtomicUsize::new(0)),
            spill_threshold_bytes: AtomicUsize::new(spill_threshold_bytes),
            max_disk_bytes: AtomicUsize::new(max_disk_bytes),
        })
    }

    /// Return current disk bytes used by spilled blobs.
    #[inline]
    pub fn used_disk_bytes(&self) -> usize {
        self.used_disk_bytes.load(Ordering::Relaxed)
    }

    /// Get write-time spill threshold in bytes (0 = disabled).
    #[inline]
    pub fn spill_threshold_bytes(&self) -> usize {
        self.spill_threshold_bytes.load(Ordering::Relaxed)
    }

    /// Update write-time spill threshold in bytes (0 = disabled).
    pub fn set_spill_threshold_bytes(&self, bytes: usize) {
        self.spill_threshold_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Get maximum disk bytes for blob storage (0 = unlimited).
    #[inline]
    pub fn max_disk_bytes(&self) -> usize {
        self.max_disk_bytes.load(Ordering::Relaxed)
    }

    /// Set maximum disk bytes for blob storage (0 = unlimited).
    pub fn set_max_disk_bytes(&self, bytes: usize) {
        self.max_disk_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Check whether `len` additional bytes fit within `max_disk_bytes`.
    #[inline]
    pub fn can_fit_on_disk(&self, len: usize) -> bool {
        let max = self.max_disk_bytes.load(Ordering::Relaxed);
        max == 0 || self.used_disk_bytes().saturating_add(len) <= max
    }

    /// Check whether a value should be spilled immediately on write.
    #[inline]
    pub fn should_spill_on_write(&self, value: &KvValue) -> bool {
        let threshold = self.spill_threshold_bytes.load(Ordering::Relaxed);
        if threshold == 0 {
            return false;
        }
        let len = match value {
            KvValue::Bytes(b) => b.len(),
            KvValue::String(s) => s.len(),
            _ => return false,
        };
        len >= threshold && self.can_fit_on_disk(len)
    }

    /// Write a `KvValue::Bytes` or `KvValue::String` payload to disk and return
    /// its [`DiskBlob`] handle. Returns `Ok(None)` if the value type is not
    /// spillable or disk quota would be exceeded.
    pub fn store_value(&self, value: &KvValue) -> Result<Option<Arc<DiskBlob>>> {
        let (bytes, is_string) = match value {
            KvValue::Bytes(b) => (b.as_slice(), false),
            KvValue::String(s) => (s.as_bytes(), true),
            _ => return Ok(None),
        };

        let len = bytes.len();
        if !self.can_fit_on_disk(len) {
            return Ok(None);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let path = self.blob_dir.join(format!("blob-{:016x}.dat", id));
        let crc32 = crc32fast::hash(bytes);

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(bytes)?;
        file.flush()?;

        self.used_disk_bytes.fetch_add(len, Ordering::Relaxed);
        debug!("Spilled {} bytes to {}", len, path.display());

        Ok(Some(Arc::new(DiskBlob {
            id,
            path,
            len,
            is_string,
            crc32,
            used_disk_bytes: Arc::clone(&self.used_disk_bytes),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_blob_store_write_read_and_drop_cleanup() {
        let temp_dir = TempDir::new().unwrap();
        let store = BlobStore::new(temp_dir.path(), 1024, 10 * 1024).unwrap();

        let payload = vec![42u8; 2048];
        let val = KvValue::Bytes(payload.clone());
        assert!(store.should_spill_on_write(&val));

        let blob = store.store_value(&val).unwrap().expect("should spill");
        let blob_path = blob.path.clone();
        assert!(blob_path.exists());
        assert_eq!(store.used_disk_bytes(), 2048);

        let restored = blob.read_value().unwrap();
        assert_eq!(restored, val);

        // Dropping the Arc<DiskBlob> must delete the file and reclaim disk quota
        drop(blob);
        assert!(!blob_path.exists());
        assert_eq!(store.used_disk_bytes(), 0);
    }

    #[test]
    fn test_blob_store_respects_max_disk_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let store = BlobStore::new(temp_dir.path(), 100, 500).unwrap();

        let v1 = KvValue::String("a".repeat(300));
        let b1 = store.store_value(&v1).unwrap();
        assert!(b1.is_some());
        assert_eq!(store.used_disk_bytes(), 300);

        // Second 300-byte blob exceeds max_disk_bytes (500)
        let v2 = KvValue::String("b".repeat(300));
        assert!(!store.should_spill_on_write(&v2));
        assert!(store.store_value(&v2).unwrap().is_none());
    }
}
