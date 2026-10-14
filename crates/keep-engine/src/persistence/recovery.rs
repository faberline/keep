use super::format::WalOp;
use super::snapshot::SnapshotLoader;
use super::wal::{find_wal_files, WalReader};
///! Recovery orchestration
///!
///! Coordinates loading snapshots and replaying WAL on startup.
use super::{PersistenceError, RecoveryStats, Result};
use crate::engine::{Entry, KvEngine};
use std::path::Path;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Recovery manager for loading persisted state
pub struct RecoveryManager;

impl RecoveryManager {
    /// Recover engine state from snapshot and WAL files
    ///
    /// Recovery process:
    /// 1. Create empty engine with specified number of shards
    /// 2. Load latest snapshot (if exists) into engine
    /// 3. Find all WAL files created since snapshot
    /// 4. Replay WAL entries to bring engine to latest state
    /// 5. Skip corrupted entries with warnings
    ///
    /// Returns the recovered engine and recovery statistics.
    pub fn recover(
        data_dir: impl AsRef<Path>,
        num_shards: usize,
    ) -> Result<(KvEngine, RecoveryStats)> {
        let engine = KvEngine::with_shards(num_shards);
        let stats = Self::recover_into(&engine, data_dir)?;
        Ok((engine, stats))
    }

    /// Recover state from snapshot and WAL files into an existing configured `KvEngine`.
    pub fn recover_into(engine: &KvEngine, data_dir: impl AsRef<Path>) -> Result<RecoveryStats> {
        let data_dir = data_dir.as_ref();
        let start = Instant::now();
        let num_shards = engine.num_shards();

        info!("Starting recovery from {}", data_dir.display());

        let mut snapshot_entries = 0;
        let mut wal_entries_replayed = 0;
        let mut corrupted_entries = 0;
        let mut wal_position_start = 0u64;

        // Step 1: Load latest snapshot (if exists)
        if let Some((snapshot_data, wal_position)) = SnapshotLoader::load_latest(data_dir)? {
            info!(
                "Found snapshot with {} entries at WAL position {}",
                snapshot_data.total_entries, wal_position
            );

            // Validate shard count matches
            if snapshot_data.shards.len() != num_shards {
                warn!(
                    "Snapshot has {} shards but engine configured for {}",
                    snapshot_data.shards.len(),
                    num_shards
                );
            }

            // Restore each shard
            for shard_data in snapshot_data.shards {
                let shard_id = shard_data.shard_id as usize;

                // Convert serializable entries back to runtime entries
                let entries: std::collections::HashMap<String, Entry> = shard_data
                    .entries
                    .into_iter()
                    .map(|(key, serializable_entry)| (key, serializable_entry.to_entry()))
                    .collect();

                snapshot_entries += entries.len();

                // Import into engine
                if !engine.import_shard(shard_id, entries) {
                    warn!("Failed to import shard {}", shard_id);
                }
            }

            wal_position_start = wal_position;
            info!("Loaded {} entries from snapshot", snapshot_entries);
        } else {
            info!("No snapshot found, starting from empty state");
        }

        // Step 2: Find and replay WAL files
        let wal_files = find_wal_files(data_dir)?;

        if wal_files.is_empty() {
            info!("No WAL files found");
        } else {
            info!("Found {} WAL file(s) to replay", wal_files.len());

            for (idx, wal_path) in wal_files.into_iter().enumerate() {
                debug!("Replaying WAL: {}", wal_path.display());

                // Only the WAL segment that was active when the snapshot was taken
                // starts at `wal_position_start`; any segments rotated after that
                // must be replayed from their beginning (offset 0).
                let skip_before = if idx == 0 { wal_position_start } else { 0 };

                match Self::replay_wal(engine, &wal_path, skip_before) {
                    Ok((replayed, corrupted)) => {
                        wal_entries_replayed += replayed;
                        corrupted_entries += corrupted;
                    }
                    Err(e) => {
                        warn!("Failed to replay WAL {}: {}", wal_path.display(), e);
                        // Continue with other WAL files
                    }
                }
            }

            info!(
                "Replayed {} WAL entries ({} corrupted/skipped)",
                wal_entries_replayed, corrupted_entries
            );
        }

        let recovery_duration = start.elapsed();
        let stats = RecoveryStats {
            snapshot_loaded: snapshot_entries > 0,
            snapshot_entries,
            wal_entries_replayed,
            corrupted_entries,
            recovery_duration,
        };

        info!(
            "Recovery complete in {:?}: {} total entries ({} from snapshot + {} from WAL)",
            recovery_duration,
            snapshot_entries + wal_entries_replayed,
            snapshot_entries,
            wal_entries_replayed
        );

        Ok(stats)
    }

    /// Replay a single WAL file
    ///
    /// Returns (entries_replayed, corrupted_entries)
    fn replay_wal(
        engine: &KvEngine,
        wal_path: impl AsRef<Path>,
        skip_before_position: u64,
    ) -> Result<(usize, usize)> {
        let mut reader = WalReader::new(wal_path)?;
        let mut replayed = 0;
        let mut corrupted = 0;

        loop {
            // Skip entries before snapshot position
            if reader.position() < skip_before_position {
                match reader.read_entry() {
                    Ok(Some(_)) => continue,
                    Ok(None) => break,
                    Err(_) => {
                        corrupted += 1;
                        continue;
                    }
                }
            }

            // Read and apply entry
            match reader.read_entry() {
                Ok(Some(entry)) => {
                    // Apply operation to engine
                    if let Err(e) = Self::apply_wal_operation(engine, &entry.op) {
                        warn!("Failed to apply WAL operation: {}", e);
                        corrupted += 1;
                    } else {
                        replayed += 1;
                    }
                }
                Ok(None) => {
                    // End of file
                    break;
                }
                Err(e) => {
                    // Corrupted entry - log and skip
                    warn!("Corrupted WAL entry: {}", e);
                    corrupted += 1;
                    // Try to continue reading
                }
            }
        }

        Ok((replayed, corrupted))
    }

    /// Apply a single WAL operation to the engine
    /// Public entry point to apply a single logical op to an engine — used by
    /// the raft state machine to replay committed commands (same path as WAL
    /// recovery).
    pub fn apply_one(engine: &KvEngine, op: &WalOp) -> Result<()> {
        Self::apply_wal_operation(engine, op)
    }

    fn apply_wal_operation(engine: &KvEngine, op: &WalOp) -> Result<()> {
        use crate::types::KvKey;

        match op {
            WalOp::Set { key, value, ttl } => {
                let kv_key = KvKey::new(key).map_err(|e| PersistenceError::CorruptedWal {
                    pos: 0,
                    reason: format!("Invalid key: {}", e),
                })?;
                let _ = engine.set(&kv_key, value.clone(), *ttl); // Ignore OOM during recovery
            }

            WalOp::Delete { key } => {
                let kv_key = KvKey::new(key).map_err(|e| PersistenceError::CorruptedWal {
                    pos: 0,
                    reason: format!("Invalid key: {}", e),
                })?;
                engine.delete(&kv_key);
            }

            WalOp::Incr { key, delta } => {
                let kv_key = KvKey::new(key).map_err(|e| PersistenceError::CorruptedWal {
                    pos: 0,
                    reason: format!("Invalid key: {}", e),
                })?;
                let _ = engine.incr(&kv_key, *delta); // Ignore errors during recovery
            }

            WalOp::Decr { key, delta } => {
                let kv_key = KvKey::new(key).map_err(|e| PersistenceError::CorruptedWal {
                    pos: 0,
                    reason: format!("Invalid key: {}", e),
                })?;
                let _ = engine.decr(&kv_key, *delta); // Ignore errors during recovery
            }

            WalOp::MSet { pairs, ttl } => {
                // Apply each pair individually
                for (key, value) in pairs {
                    if let Ok(kv_key) = KvKey::new(key) {
                        let _ = engine.set(&kv_key, value.clone(), *ttl); // Ignore OOM during recovery
                    }
                }
            }

            WalOp::MDel { keys } => {
                for key in keys {
                    if let Ok(kv_key) = KvKey::new(key) {
                        engine.delete(&kv_key);
                    }
                }
            }

            WalOp::SetNx { key, value, ttl } => {
                let kv_key = KvKey::new(key).map_err(|e| PersistenceError::CorruptedWal {
                    pos: 0,
                    reason: format!("Invalid key: {}", e),
                })?;
                let _ = engine.setnx(&kv_key, value.clone(), *ttl); // Ignore OOM during recovery
            }

            WalOp::Lock { key, owner, ttl } => {
                let kv_key = KvKey::new(key).map_err(|e| PersistenceError::CorruptedWal {
                    pos: 0,
                    reason: format!("Invalid key: {}", e),
                })?;
                engine.lock(&kv_key, owner, *ttl);
            }

            WalOp::Unlock { key, owner } => {
                let kv_key = KvKey::new(key).map_err(|e| PersistenceError::CorruptedWal {
                    pos: 0,
                    reason: format!("Invalid key: {}", e),
                })?;
                let _ = engine.unlock(&kv_key, owner); // Ignore errors during recovery
            }

            WalOp::ExtendLock { key, owner, ttl } => {
                let kv_key = KvKey::new(key).map_err(|e| PersistenceError::CorruptedWal {
                    pos: 0,
                    reason: format!("Invalid key: {}", e),
                })?;
                let _ = engine.extend_lock(&kv_key, owner, *ttl); // Ignore errors during recovery
            }

            // --- collection / expiry / cas ops (errors ignored during replay) ---
            WalOp::Cas {
                key,
                expected,
                new,
                ttl,
            } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.cas(&k, expected, new.clone(), *ttl);
                }
            }
            WalOp::Expire { key, seconds } => {
                if let Ok(k) = KvKey::new(key) {
                    engine.expire(&k, *seconds);
                }
            }
            WalOp::PExpire { key, milliseconds } => {
                if let Ok(k) = KvKey::new(key) {
                    engine.pexpire(&k, *milliseconds);
                }
            }
            WalOp::Persist { key } => {
                if let Ok(k) = KvKey::new(key) {
                    engine.persist(&k);
                }
            }
            WalOp::HSet { key, fields } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.hset(&k, fields.clone());
                }
            }
            WalOp::HDel { key, fields } => {
                if let Ok(k) = KvKey::new(key) {
                    let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
                    let _ = engine.hdel(&k, &refs);
                }
            }
            WalOp::HIncrBy { key, field, delta } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.hincrby(&k, field, *delta);
                }
            }
            WalOp::HIncrByFloat { key, field, delta } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.hincrbyfloat(&k, field, *delta);
                }
            }
            WalOp::SAdd { key, members } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.sadd(&k, members.clone());
                }
            }
            WalOp::SRem { key, members } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.srem(&k, members.clone());
                }
            }
            WalOp::ZAdd { key, members } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.zadd(&k, members.clone());
                }
            }
            WalOp::ZRem { key, members } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.zrem(&k, members.clone());
                }
            }
            WalOp::ZIncrBy { key, member, delta } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.zincrby(&k, member, *delta);
                }
            }
            WalOp::LPush { key, values } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.lpush(&k, values.clone());
                }
            }
            WalOp::RPush { key, values } => {
                if let Ok(k) = KvKey::new(key) {
                    let _ = engine.rpush(&k, values.clone());
                }
            }
            WalOp::LPop { key } => {
                if let Ok(k) = KvKey::new(key) {
                    engine.lpop(&k);
                }
            }
            WalOp::RPop { key } => {
                if let Ok(k) = KvKey::new(key) {
                    engine.rpop(&k);
                }
            }
            WalOp::GetEx {
                key,
                ttl_ms,
                persist,
            } => {
                if let Ok(k) = KvKey::new(key) {
                    let ttl = ttl_ms.map(std::time::Duration::from_millis);
                    engine.getex(&k, ttl, *persist);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::wal::WalWriter;
    use crate::persistence::WalConfig;
    use crate::types::KvKey;
    use crate::KvValue;
    use tempfile::TempDir;

    #[test]
    fn test_recovery_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        let (engine, stats) = RecoveryManager::recover(data_dir, 256).unwrap();

        assert_eq!(stats.snapshot_entries, 0);
        assert_eq!(stats.wal_entries_replayed, 0);
        assert_eq!(stats.corrupted_entries, 0);
        assert_eq!(engine.len(), 0);
    }

    #[test]
    fn test_recovery_with_wal() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        // Write some WAL entries
        let config = WalConfig::default();
        let mut wal_writer = WalWriter::new(data_dir.to_path_buf(), config).unwrap();

        use super::super::format::WalOp;
        wal_writer
            .append(WalOp::Set {
                key: "key1".to_string(),
                value: KvValue::String("value1".to_string()),
                ttl: None,
            })
            .unwrap();

        wal_writer
            .append(WalOp::Set {
                key: "key2".to_string(),
                value: KvValue::Int(42),
                ttl: None,
            })
            .unwrap();

        wal_writer.flush().unwrap();
        drop(wal_writer);

        // Recover
        let (engine, stats) = RecoveryManager::recover(data_dir, 256).unwrap();

        assert_eq!(stats.snapshot_entries, 0);
        assert_eq!(stats.wal_entries_replayed, 2);
        assert_eq!(stats.corrupted_entries, 0);

        // Verify data was restored
        let key1 = KvKey::new("key1").unwrap();
        let key2 = KvKey::new("key2").unwrap();

        assert_eq!(
            engine.get(&key1),
            Some(KvValue::String("value1".to_string()))
        );
        assert_eq!(engine.get(&key2), Some(KvValue::Int(42)));
    }

    #[test]
    fn test_recovery_batch_operations() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        // Write batch operations
        let config = WalConfig::default();
        let mut wal_writer = WalWriter::new(data_dir.to_path_buf(), config).unwrap();

        use super::super::format::WalOp;

        wal_writer
            .append(WalOp::MSet {
                pairs: vec![
                    ("k1".to_string(), KvValue::String("v1".to_string())),
                    ("k2".to_string(), KvValue::String("v2".to_string())),
                ],
                ttl: None,
            })
            .unwrap();

        wal_writer
            .append(WalOp::MDel {
                keys: vec!["k1".to_string()],
            })
            .unwrap();

        wal_writer.flush().unwrap();
        drop(wal_writer);

        // Recover
        let (engine, stats) = RecoveryManager::recover(data_dir, 256).unwrap();

        assert_eq!(stats.wal_entries_replayed, 2);

        // k1 should be deleted, k2 should exist
        let k1 = KvKey::new("k1").unwrap();
        let k2 = KvKey::new("k2").unwrap();

        assert_eq!(engine.get(&k1), None);
        assert_eq!(engine.get(&k2), Some(KvValue::String("v2".to_string())));
    }

    #[test]
    fn test_recovery_snapshot_preserves_ttl() {
        use crate::persistence::snapshot::SnapshotWriter;
        use crate::persistence::SnapshotConfig;
        use std::time::Duration;

        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        let engine = KvEngine::with_shards(16);
        let ttl_key = KvKey::new("session_token").unwrap();
        let permanent_key = KvKey::new("config_key").unwrap();

        engine
            .set(
                &ttl_key,
                KvValue::String("secret".to_string()),
                Some(Duration::from_secs(120)),
            )
            .unwrap();
        engine.set(&permanent_key, KvValue::Int(99), None).unwrap();

        let snap_writer = SnapshotWriter::new(SnapshotConfig::default());
        snap_writer.create_snapshot(&engine, data_dir, 0).unwrap();

        let (recovered, stats) = RecoveryManager::recover(data_dir, 16).unwrap();
        assert!(stats.snapshot_loaded);
        assert_eq!(stats.snapshot_entries, 2);

        // Both keys must be live after recovery, and ttl_key must have ~120s remaining TTL
        assert_eq!(
            recovered.get(&ttl_key),
            Some(KvValue::String("secret".to_string()))
        );
        let remaining_ttl = recovered.ttl(&ttl_key);
        assert!(
            remaining_ttl >= 110 && remaining_ttl <= 120,
            "Expected ~120s TTL after snapshot recovery, got {}",
            remaining_ttl
        );
        assert_eq!(recovered.get(&permanent_key), Some(KvValue::Int(99)));
        assert_eq!(recovered.ttl(&permanent_key), -1);
    }

    #[test]
    fn test_recovery_snapshot_plus_rotated_wals() {
        use super::super::format::WalOp;
        use crate::persistence::snapshot::SnapshotWriter;
        use crate::persistence::wal::find_wal_files;
        use crate::persistence::SnapshotConfig;

        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        let engine = KvEngine::with_shards(16);
        let mut wal_writer = WalWriter::new(data_dir.to_path_buf(), WalConfig::default()).unwrap();

        // 1. Pre-snapshot writes + pre-snapshot WAL rotation
        for i in 0..5 {
            let k = format!("pre_snap_{}", i);
            let kv_k = KvKey::new(&k).unwrap();
            engine.set(&kv_k, KvValue::Int(i), None).unwrap();
            wal_writer
                .append(WalOp::Set {
                    key: k,
                    value: KvValue::Int(i),
                    ttl: None,
                })
                .unwrap();
        }
        wal_writer.rotate().unwrap();

        // Write one more key to wal-current.log before taking snapshot
        let k_active = KvKey::new("in_current_before_snap").unwrap();
        engine
            .set(&k_active, KvValue::String("snap_val".repeat(20)), None)
            .unwrap();
        wal_writer
            .append(WalOp::Set {
                key: "in_current_before_snap".to_string(),
                value: KvValue::String("snap_val".repeat(20)),
                ttl: None,
            })
            .unwrap();
        wal_writer.flush().unwrap();

        // 2. Take snapshot at current wal_writer.position() (which is > 200 bytes)
        let snap_pos = wal_writer.position();
        assert!(snap_pos > 150);
        let snap_writer = SnapshotWriter::new(SnapshotConfig::default());
        snap_writer
            .create_snapshot(&engine, data_dir, snap_pos)
            .unwrap();

        // Pre-snapshot rotated WAL must have been cleaned up by create_snapshot
        assert_eq!(find_wal_files(data_dir).unwrap().len(), 1);

        // 3. Post-snapshot writes across multiple WAL rotations (where new WAL files
        // have entries at byte offsets < snap_pos!)
        wal_writer
            .append(WalOp::Set {
                key: "post_snap_before_rotate".to_string(),
                value: KvValue::Int(100),
                ttl: None,
            })
            .unwrap();
        wal_writer.rotate().unwrap();

        wal_writer
            .append(WalOp::Set {
                key: "post_rotate_1".to_string(),
                value: KvValue::Int(200),
                ttl: None,
            })
            .unwrap();
        wal_writer.rotate().unwrap();

        wal_writer
            .append(WalOp::Set {
                key: "post_rotate_2".to_string(),
                value: KvValue::Int(300),
                ttl: None,
            })
            .unwrap();
        wal_writer.flush().unwrap();
        drop(wal_writer);

        // 4. Recover and verify all pre-snapshot and post-rotation keys are intact!
        let (recovered, stats) = RecoveryManager::recover(data_dir, 16).unwrap();
        assert!(stats.snapshot_loaded);
        assert_eq!(stats.snapshot_entries, 6);
        assert_eq!(stats.wal_entries_replayed, 3);

        for i in 0..5 {
            let k = KvKey::new(format!("pre_snap_{}", i)).unwrap();
            assert_eq!(recovered.get(&k), Some(KvValue::Int(i)));
        }
        assert_eq!(
            recovered.get(&KvKey::new("post_snap_before_rotate").unwrap()),
            Some(KvValue::Int(100))
        );
        assert_eq!(
            recovered.get(&KvKey::new("post_rotate_1").unwrap()),
            Some(KvValue::Int(200))
        );
        assert_eq!(
            recovered.get(&KvKey::new("post_rotate_2").unwrap()),
            Some(KvValue::Int(300))
        );
    }
}
