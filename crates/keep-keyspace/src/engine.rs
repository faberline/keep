//! Sharded KV storage engine
//!
//! Partitions keyspace into multiple shards for multi-core scalability.
//! Each shard uses RwLock for concurrent reads and exclusive writes.

use crate::error::KvError;
use crate::types::{KvKey, KvValue};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

mod entry;
mod eviction;
mod kv_batch_ttl;
mod kv_collections;
mod shard_hash;
mod shard_list;
mod shard_set;
mod shard_zset;

pub use entry::{Entry, EvictionCandidate};
pub use eviction::EvictionPolicy;

/// Default number of shards (power of 2 for efficient modulo)
const DEFAULT_NUM_SHARDS: usize = 256;

/// Process-wide monotonic clock epoch for lock-free nanosecond access tracking
#[inline]
fn engine_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Global monotonic counter combined with elapsed nanoseconds to guarantee strict
/// ordering even when two accesses occur within the same CPU clock tick.
#[inline]
fn now_access_stamp() -> u64 {
    static TICK: AtomicU64 = AtomicU64::new(1);
    let nanos = (engine_epoch().elapsed().as_nanos() as u64) << 16;
    let seq = TICK.fetch_add(1, Ordering::Relaxed) & 0xFFFF;
    nanos | seq
}

/// A single shard containing a portion of the keyspace
pub struct Shard {
    data: RwLock<HashMap<String, Entry>>,
    /// O(1) incremental resident RAM usage in bytes for this shard
    used_memory_bytes: AtomicUsize,
}

impl Shard {
    /// Create a new empty shard
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
            used_memory_bytes: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn compute_entry_ram(key_len: usize, entry: &Entry) -> usize {
        let val_size = if entry.spilled.is_some() {
            32 // Arc<DiskBlob> metadata overhead in RAM
        } else {
            Self::estimate_value_size(&entry.value)
        };
        key_len + val_size + 64
    }

    #[inline]
    fn add_mem(&self, bytes: usize) {
        self.used_memory_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    fn sub_mem(&self, bytes: usize) {
        let _ = self
            .used_memory_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(bytes))
            });
    }

    #[inline]
    fn insert_entry(
        &self,
        guard: &mut HashMap<String, Entry>,
        key: String,
        mut entry: Entry,
    ) -> Option<Entry> {
        let ram = Self::compute_entry_ram(key.len(), &entry);
        entry.ram_bytes = ram;
        self.add_mem(ram);
        let prev = guard.insert(key, entry);
        if let Some(ref old) = prev {
            self.sub_mem(old.ram_bytes);
        }
        prev
    }

    #[inline]
    fn remove_entry(&self, guard: &mut HashMap<String, Entry>, key: &str) -> Option<Entry> {
        let removed = guard.remove(key);
        if let Some(ref old) = removed {
            self.sub_mem(old.ram_bytes);
        }
        removed
    }

    #[inline]
    fn refresh_entry_ram(&self, key_len: usize, entry: &mut Entry) {
        let old_ram = entry.ram_bytes;
        let new_ram = Self::compute_entry_ram(key_len, entry);
        entry.ram_bytes = new_ram;
        if new_ram >= old_ram {
            self.add_mem(new_ram - old_ram);
        } else {
            self.sub_mem(old_ram - new_ram);
        }
    }

    /// Get a value by key (returns None if expired).
    /// Updates LRU/LFU atomically under the shared read lock and materializes
    /// any disk-spilled value *outside* the shard lock.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let cloned = {
            let guard = self.data.read();
            guard.get(key).and_then(|entry| {
                if entry.is_expired() {
                    None
                } else {
                    entry.touch();
                    Some(entry.clone())
                }
            })
        };
        cloned.and_then(Entry::materialize_into)
    }

    /// Set a value with optional TTL
    pub fn set(&self, key: String, value: KvValue, ttl: Option<Duration>) -> Option<Entry> {
        self.set_entry(key, Entry::new(value, ttl))
    }

    /// Set a pre-constructed Entry (in-memory or disk-spilled)
    pub fn set_entry(&self, key: String, entry: Entry) -> Option<Entry> {
        let mut guard = self.data.write();
        if let Some(existing) = guard.get(&key) {
            if existing.is_expired() {
                self.remove_entry(&mut guard, &key);
            }
        }
        self.insert_entry(&mut guard, key, entry)
    }

    /// Delete a key, returns the old entry if existed
    pub fn delete(&self, key: &str) -> Option<Entry> {
        let mut guard = self.data.write();
        self.remove_entry(&mut guard, key)
    }

    /// Check if key exists (and not expired)
    pub fn exists(&self, key: &str) -> bool {
        let guard = self.data.read();
        guard.get(key).map(|e| !e.is_expired()).unwrap_or(false)
    }

    /// Atomic increment for Int values
    pub fn incr(&self, key: &str, delta: i64) -> Result<i64, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                if entry.spilled.is_some() {
                    return Err(KvError::TypeMismatch {
                        expected: "Int".to_string(),
                        actual: "SpilledBlob".to_string(),
                    });
                }
                match &mut entry.value {
                    KvValue::Int(n) => {
                        *n = n.saturating_add(delta);
                        let res = *n;
                        entry.version += 1;
                        entry.touch();
                        Ok(res)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "Int".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::Int(delta), None),
                );
                Ok(delta)
            }
        }
    }

    /// Compare-And-Swap: atomically update if current value matches expected
    pub fn cas(
        &self,
        key: &str,
        expected: &KvValue,
        new_value: KvValue,
        ttl: Option<Duration>,
    ) -> Result<bool, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                let current_matches = if let Some(blob) = &entry.spilled {
                    blob.read_value().ok().as_ref() == Some(expected)
                } else {
                    &entry.value == expected
                };
                if current_matches {
                    entry.spilled = None;
                    entry.value = new_value;
                    entry.version += 1;
                    entry.touch();
                    if let Some(d) = ttl {
                        entry.expires_at = Some(Instant::now() + d);
                    }
                    self.refresh_entry_ram(key.len(), entry);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Err(KvError::KeyNotFound(key.to_string())),
        }
    }

    /// Get entry count (including expired - for stats)
    pub fn len(&self) -> usize {
        self.data.read().len()
    }

    /// Check if shard is empty
    pub fn is_empty(&self) -> bool {
        self.data.read().is_empty()
    }

    /// Remove all expired entries, returns count removed
    pub fn cleanup_expired(&self) -> usize {
        let mut guard = self.data.write();
        let before = guard.len();
        let mut freed_ram = 0usize;
        guard.retain(|_, entry| {
            if entry.is_expired() {
                freed_ram += entry.ram_bytes;
                false
            } else {
                true
            }
        });
        if freed_ram > 0 {
            self.sub_mem(freed_ram);
        }
        before - guard.len()
    }

    /// Set if not exists (atomic)
    pub fn setnx(&self, key: String, value: KvValue, ttl: Option<Duration>) -> bool {
        self.setnx_entry(key, Entry::new(value, ttl))
    }

    /// Set pre-constructed entry if key does not exist
    pub fn setnx_entry(&self, key: String, entry: Entry) -> bool {
        let mut guard = self.data.write();

        if let Some(existing) = guard.get(&key) {
            if !existing.is_expired() {
                return false;
            }
        }

        self.insert_entry(&mut guard, key, entry);
        true
    }

    /// Acquire a lock with owner ID and TTL
    pub fn lock(&self, key: String, owner: String, ttl: Duration) -> bool {
        self.setnx(key, KvValue::String(owner), Some(ttl))
    }

    /// Release a lock (only if owned)
    pub fn unlock(&self, key: &str, owner: &str) -> Result<bool, KvError> {
        let mut guard = self.data.write();

        match guard.get(key) {
            Some(entry) if !entry.is_expired() => match &entry.value {
                KvValue::String(stored_owner) if stored_owner == owner => {
                    self.remove_entry(&mut guard, key);
                    Ok(true)
                }
                KvValue::String(stored_owner) => Err(KvError::LockOwnerMismatch {
                    expected: owner.to_string(),
                    actual: stored_owner.clone(),
                }),
                _ => Err(KvError::TypeMismatch {
                    expected: "String (lock owner)".to_string(),
                    actual: "other type".to_string(),
                }),
            },
            _ => Ok(false),
        }
    }

    /// Extend lock TTL (only if owned)
    pub fn extend_lock(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => match &entry.value {
                KvValue::String(stored_owner) if stored_owner == owner => {
                    entry.expires_at = Some(Instant::now() + ttl);
                    entry.version += 1;
                    entry.touch();
                    Ok(true)
                }
                KvValue::String(stored_owner) => Err(KvError::LockOwnerMismatch {
                    expected: owner.to_string(),
                    actual: stored_owner.clone(),
                }),
                _ => Err(KvError::TypeMismatch {
                    expected: "String (lock owner)".to_string(),
                    actual: "other type".to_string(),
                }),
            },
            _ => Ok(false),
        }
    }

    /// Scan keys with optional prefix filter
    pub fn scan(&self, prefix: Option<&str>, limit: usize) -> Vec<String> {
        let guard = self.data.read();
        guard
            .iter()
            .filter(|(key, entry)| {
                if entry.is_expired() {
                    return false;
                }
                match prefix {
                    Some(p) => key.starts_with(p),
                    None => true,
                }
            })
            .take(limit)
            .map(|(key, _)| key.clone())
            .collect()
    }

    // ==================== TTL Management ====================

    /// Set TTL for a key (EXPIRE/PEXPIRE)
    /// Returns 1 if TTL was set, 0 if key doesn't exist
    pub fn expire(&self, key: &str, ttl: Duration) -> i32 {
        let mut guard = self.data.write();
        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.expires_at = Some(Instant::now() + ttl);
                entry.version += 1;
                1
            }
            _ => 0,
        }
    }

    /// Get remaining TTL in seconds
    /// Returns -2 if key doesn't exist, -1 if no TTL, or remaining seconds
    pub fn ttl(&self, key: &str) -> i64 {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => match entry.expires_at {
                Some(exp) => {
                    let now = Instant::now();
                    if exp > now {
                        (exp - now).as_secs() as i64
                    } else {
                        0
                    }
                }
                None => -1,
            },
            _ => -2,
        }
    }

    /// Get remaining TTL in milliseconds
    /// Returns -2 if key doesn't exist, -1 if no TTL, or remaining milliseconds
    pub fn pttl(&self, key: &str) -> i64 {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => match entry.expires_at {
                Some(exp) => {
                    let now = Instant::now();
                    if exp > now {
                        (exp - now).as_millis() as i64
                    } else {
                        0
                    }
                }
                None => -1,
            },
            _ => -2,
        }
    }

    /// Remove TTL from a key (PERSIST)
    /// Returns 1 if TTL was removed, 0 if key doesn't exist or has no TTL
    pub fn persist(&self, key: &str) -> i32 {
        let mut guard = self.data.write();
        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                if entry.expires_at.is_some() {
                    entry.expires_at = None;
                    entry.version += 1;
                    1
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Get value and optionally update TTL (GETEX)
    pub fn getex(&self, key: &str, ttl: Option<Duration>, persist: bool) -> Option<Entry> {
        let cloned = if ttl.is_none() && !persist {
            let guard = self.data.read();
            guard.get(key).and_then(|entry| {
                if entry.is_expired() {
                    None
                } else {
                    entry.touch();
                    Some(entry.clone())
                }
            })
        } else {
            let mut guard = self.data.write();
            match guard.get_mut(key) {
                Some(entry) if !entry.is_expired() => {
                    entry.touch();
                    if persist {
                        entry.expires_at = None;
                        entry.version += 1;
                    } else if let Some(d) = ttl {
                        entry.expires_at = Some(Instant::now() + d);
                        entry.version += 1;
                    }
                    Some(entry.clone())
                }
                _ => None,
            }
        };
        cloned.and_then(Entry::materialize_into)
    }
}

impl Default for Shard {
    fn default() -> Self {
        Self::new()
    }
}

/// High-performance sharded KV engine
pub struct KvEngine {
    shards: Vec<Shard>,
    num_shards: usize,
    /// Optional persistence handle for WAL and snapshots.
    persistence: arc_swap::ArcSwapOption<crate::persistence::handle::PersistenceHandle>,
    /// Lock-free fast-path gate for the write hot path.
    persistence_enabled: AtomicBool,
    /// Optional disk blob store for tiered storage (`/data/blobs/`).
    blob_store: arc_swap::ArcSwapOption<crate::persistence::blob_store::BlobStore>,
    /// Lock-free fast-path gate for blob tiering.
    blob_store_enabled: AtomicBool,
    /// Maximum memory limit (0 = unlimited).
    maxmemory: AtomicUsize,
    /// Eviction policy when maxmemory is reached
    eviction_policy: RwLock<EvictionPolicy>,
    /// Rotating shard cursor for O(1) approximated eviction sampling
    eviction_cursor: AtomicUsize,
    /// Total number of keys evicted due to memory pressure
    evicted_keys: AtomicU64,
    /// Total number of keys spilled to disk
    spilled_keys: AtomicU64,
}

impl KvEngine {
    /// Create a new KV engine with default number of shards (256)
    pub fn new() -> Self {
        Self::with_shards(DEFAULT_NUM_SHARDS)
    }

    /// Create a new KV engine with specified number of shards
    pub fn with_shards(num_shards: usize) -> Self {
        let num_shards = num_shards.max(1);
        let shards = (0..num_shards).map(|_| Shard::new()).collect();
        Self {
            shards,
            num_shards,
            persistence: arc_swap::ArcSwapOption::empty(),
            persistence_enabled: AtomicBool::new(false),
            blob_store: arc_swap::ArcSwapOption::empty(),
            blob_store_enabled: AtomicBool::new(false),
            maxmemory: AtomicUsize::new(0),
            eviction_policy: RwLock::new(EvictionPolicy::default()),
            eviction_cursor: AtomicUsize::new(0),
            evicted_keys: AtomicU64::new(0),
            spilled_keys: AtomicU64::new(0),
        }
    }

    /// Enable persistence on this engine
    pub fn enable_persistence(
        &self,
        persistence_handle: std::sync::Arc<crate::persistence::handle::PersistenceHandle>,
    ) {
        self.persistence.store(Some(persistence_handle));
        self.persistence_enabled.store(true, Ordering::Release);
    }

    /// Whether writes go through the WAL (durable-before-ack) or stay RAM-only.
    pub fn persistence_enabled(&self) -> bool {
        self.persistence_enabled.load(Ordering::Acquire)
    }

    /// The WAL group-commit fsync interval, when persistence is enabled.
    pub fn fsync_interval_ms(&self) -> Option<u64> {
        if !self.persistence_enabled() {
            return None;
        }
        self.persistence
            .load()
            .as_ref()
            .map(|p| p.config().wal_config.flush_interval_ms)
    }

    /// Log an operation to WAL (if persistence is enabled).
    #[inline]
    fn log_wal(&self, op: crate::persistence::format::WalOp) {
        if !self.persistence_enabled.load(Ordering::Acquire) {
            return;
        }
        if let Some(persistence) = &*self.persistence.load() {
            persistence.log_operation(op);
        }
    }

    /// Durability barrier for durable-before-ack writes.
    #[inline]
    pub fn durability_barrier(&self) -> Option<tokio::sync::oneshot::Receiver<()>> {
        if !self.persistence_enabled.load(Ordering::Acquire) {
            return None;
        }
        self.persistence.load().as_ref().and_then(|p| p.barrier())
    }

    /// Get the shard for a given key
    #[inline]
    fn shard_for_key(&self, key: &str) -> &Shard {
        let hash = Self::hash_key(key);
        let idx = hash as usize % self.num_shards;
        &self.shards[idx]
    }

    /// Hash a key to u64
    #[inline]
    fn hash_key(key: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    /// Get a value by key
    pub fn get(&self, key: &KvKey) -> Option<KvValue> {
        self.shard_for_key(key.as_str())
            .get(key.as_str())
            .map(|entry| entry.value)
    }

    /// Set a value with optional TTL
    /// Returns Ok(()) on success, Err(OutOfMemory) if maxmemory exceeded with noeviction policy
    pub fn set(&self, key: &KvKey, value: KvValue, ttl: Option<Duration>) -> Result<(), KvError> {
        self.check_memory_and_evict()?;

        self.log_wal(crate::persistence::format::WalOp::Set {
            key: key.as_str().to_string(),
            value: value.clone(),
            ttl,
        });

        let entry = self.prepare_entry(value, ttl);
        self.shard_for_key(key.as_str())
            .set_entry(key.as_str().to_string(), entry);
        Ok(())
    }

    /// Set key with expiry in seconds (SETEX)
    pub fn setex(&self, key: &KvKey, seconds: u64, value: KvValue) -> Result<(), KvError> {
        self.set(key, value, Some(Duration::from_secs(seconds)))
    }

    /// Set key with expiry in milliseconds (PSETEX)
    pub fn psetex(&self, key: &KvKey, milliseconds: u64, value: KvValue) -> Result<(), KvError> {
        self.set(key, value, Some(Duration::from_millis(milliseconds)))
    }

    /// Delete a key
    pub fn delete(&self, key: &KvKey) -> bool {
        self.log_wal(crate::persistence::format::WalOp::Delete {
            key: key.as_str().to_string(),
        });

        self.shard_for_key(key.as_str())
            .delete(key.as_str())
            .is_some()
    }

    /// Check if key exists
    pub fn exists(&self, key: &KvKey) -> bool {
        self.shard_for_key(key.as_str()).exists(key.as_str())
    }

    /// Atomic increment
    pub fn incr(&self, key: &KvKey, delta: i64) -> Result<i64, KvError> {
        self.check_memory_and_evict()?;

        self.log_wal(crate::persistence::format::WalOp::Incr {
            key: key.as_str().to_string(),
            delta,
        });

        self.shard_for_key(key.as_str()).incr(key.as_str(), delta)
    }

    /// Atomic decrement
    pub fn decr(&self, key: &KvKey, delta: i64) -> Result<i64, KvError> {
        self.check_memory_and_evict()?;

        self.log_wal(crate::persistence::format::WalOp::Decr {
            key: key.as_str().to_string(),
            delta,
        });

        self.shard_for_key(key.as_str()).incr(key.as_str(), -delta)
    }

    /// Compare-And-Swap
    pub fn cas(
        &self,
        key: &KvKey,
        expected: &KvValue,
        new_value: KvValue,
        ttl: Option<Duration>,
    ) -> Result<bool, KvError> {
        self.check_memory_and_evict()?;

        self.log_wal(crate::persistence::format::WalOp::Cas {
            key: key.as_str().to_string(),
            expected: expected.clone(),
            new: new_value.clone(),
            ttl,
        });

        self.shard_for_key(key.as_str())
            .cas(key.as_str(), expected, new_value, ttl)
    }

    /// Get total entry count across all shards
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    /// Check if engine is empty
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.is_empty())
    }

    /// Get number of shards
    pub fn num_shards(&self) -> usize {
        self.num_shards
    }

    /// Cleanup expired entries across all shards, returns total removed
    pub fn cleanup_expired(&self) -> usize {
        self.shards.iter().map(|s| s.cleanup_expired()).sum()
    }

    /// Set if not exists
    pub fn setnx(
        &self,
        key: &KvKey,
        value: KvValue,
        ttl: Option<Duration>,
    ) -> Result<bool, KvError> {
        self.check_memory_and_evict()?;

        self.log_wal(crate::persistence::format::WalOp::SetNx {
            key: key.as_str().to_string(),
            value: value.clone(),
            ttl,
        });

        let entry = self.prepare_entry(value, ttl);
        Ok(self
            .shard_for_key(key.as_str())
            .setnx_entry(key.as_str().to_string(), entry))
    }

    /// Acquire a lock
    pub fn lock(&self, key: &KvKey, owner: &str, ttl: Duration) -> bool {
        let _ = self.check_memory_and_evict();
        self.log_wal(crate::persistence::format::WalOp::Lock {
            key: key.as_str().to_string(),
            owner: owner.to_string(),
            ttl,
        });

        self.shard_for_key(key.as_str())
            .lock(key.as_str().to_string(), owner.to_string(), ttl)
    }

    /// Release a lock
    pub fn unlock(&self, key: &KvKey, owner: &str) -> Result<bool, KvError> {
        self.log_wal(crate::persistence::format::WalOp::Unlock {
            key: key.as_str().to_string(),
            owner: owner.to_string(),
        });

        self.shard_for_key(key.as_str()).unlock(key.as_str(), owner)
    }

    /// Extend lock TTL
    pub fn extend_lock(&self, key: &KvKey, owner: &str, ttl: Duration) -> Result<bool, KvError> {
        self.log_wal(crate::persistence::format::WalOp::ExtendLock {
            key: key.as_str().to_string(),
            owner: owner.to_string(),
            ttl,
        });

        self.shard_for_key(key.as_str())
            .extend_lock(key.as_str(), owner, ttl)
    }

    // ==================== Scan Operations ====================

    /// Scan keys across all shards with optional prefix filter
    ///
    /// Returns up to `limit` keys that start with the given prefix.
    /// If prefix is None, returns all keys up to limit.
    ///
    /// Note: Due to sharding, keys are not returned in sorted order.
    /// The scan iterates through shards and collects keys until the limit is reached.
    ///
    /// # Example
    /// ```
    /// use keep_keyspace::engine::KvEngine;
    /// use keep_keyspace::types::{KvKey, KvValue};
    ///
    /// let engine = KvEngine::new();
    /// let key1 = KvKey::new("user:1").unwrap();
    /// let key2 = KvKey::new("user:2").unwrap();
    /// let key3 = KvKey::new("task:1").unwrap();
    ///
    /// engine.set(&key1, KvValue::Int(1), None);
    /// engine.set(&key2, KvValue::Int(2), None);
    /// engine.set(&key3, KvValue::Int(3), None);
    ///
    /// // Scan with prefix
    /// let user_keys = engine.scan(Some("user:"), 100);
    /// assert_eq!(user_keys.len(), 2);
    ///
    /// // Scan all keys
    /// let all_keys = engine.scan(None, 100);
    /// assert_eq!(all_keys.len(), 3);
    /// ```
    pub fn scan(&self, prefix: Option<&str>, limit: usize) -> Vec<String> {
        let mut result = Vec::with_capacity(limit);
        let per_shard_limit = (limit / self.num_shards).max(1);

        for shard in &self.shards {
            if result.len() >= limit {
                break;
            }
            let remaining = limit - result.len();
            let shard_keys = shard.scan(prefix, remaining.min(per_shard_limit * 2));
            result.extend(shard_keys);
        }

        result.truncate(limit);
        result
    }

    // ==================== Persistence Support ====================

    /// Export all entries from a specific shard (for persistence/snapshots)
    pub fn export_shard(&self, shard_id: usize) -> Option<HashMap<String, Entry>> {
        if shard_id >= self.num_shards {
            return None;
        }
        Some(self.shards[shard_id].export_all())
    }

    /// Import entries into a specific shard (for recovery from snapshots)
    pub fn import_shard(&self, shard_id: usize, mut entries: HashMap<String, Entry>) -> bool {
        if shard_id >= self.num_shards {
            return false;
        }
        if self.blob_store_enabled.load(Ordering::Acquire) {
            if let Some(store) = &*self.blob_store.load() {
                for entry in entries.values_mut() {
                    if !entry.is_expired()
                        && entry.spilled.is_none()
                        && store.should_spill_on_write(&entry.value)
                    {
                        if let Ok(Some(blob)) = store.store_value(&entry.value) {
                            entry.value = KvValue::Null;
                            entry.spilled = Some(blob);
                            self.spilled_keys.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        self.shards[shard_id].import_all(entries);
        true
    }

    /// Dump every live (non-expired) key→value across all shards. Snapshot
    /// helper — TTL metadata is dropped (snapshots restore values without
    /// expiry; a timestamped Entry serialization is a follow-on).
    pub fn dump_values(&self) -> Vec<(String, KvValue)> {
        let mut out = Vec::new();
        for s in 0..self.num_shards {
            if let Some(map) = self.export_shard(s) {
                for (k, e) in map {
                    if !e.is_expired() {
                        out.push((k, e.value));
                    }
                }
            }
        }
        out
    }

    /// Restore key→value pairs (snapshot install). Overwrites existing keys; no
    /// TTL is set.
    pub fn load_values(&self, items: Vec<(String, KvValue)>) {
        for (k, v) in items {
            if let Ok(key) = KvKey::new(k) {
                let _ = self.set(&key, v, None);
            }
        }
    }
}

impl Default for KvEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod unit_suite;
