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

mod shard_hash;
mod shard_list;
mod shard_set;
mod shard_zset;

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

/// Entry in the KV store with metadata and lock-free LRU/LFU tracking
#[derive(Debug)]
pub struct Entry {
    /// The stored value (or `KvValue::Null` when payload is spilled to disk)
    pub value: KvValue,
    /// Optional reference-counted handle to a disk-spilled blob payload
    pub spilled: Option<Arc<crate::persistence::blob_store::DiskBlob>>,
    /// When the entry was created
    pub created_at: Instant,
    /// Optional expiration time (TTL)
    pub expires_at: Option<Instant>,
    /// Version for CAS operations
    pub version: u64,
    /// Cached resident RAM footprint in bytes for O(1) incremental memory accounting
    pub ram_bytes: usize,
    /// Lock-free LRU timestamp (monotonic stamp)
    pub last_accessed: AtomicU64,
    /// Lock-free LFU access counter
    pub access_count: AtomicU64,
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            spilled: self.spilled.clone(),
            created_at: self.created_at,
            expires_at: self.expires_at,
            version: self.version,
            ram_bytes: self.ram_bytes,
            last_accessed: AtomicU64::new(self.last_accessed.load(Ordering::Relaxed)),
            access_count: AtomicU64::new(self.access_count.load(Ordering::Relaxed)),
        }
    }
}

impl Entry {
    /// Create a new in-memory entry
    pub fn new(value: KvValue, ttl: Option<Duration>) -> Self {
        let now = Instant::now();
        Self {
            value,
            spilled: None,
            created_at: now,
            expires_at: ttl.map(|d| now + d),
            version: 1,
            ram_bytes: 0,
            last_accessed: AtomicU64::new(now_access_stamp()),
            access_count: AtomicU64::new(1),
        }
    }

    /// Create a new disk-spilled entry whose payload resides in [`DiskBlob`]
    pub fn new_spilled(
        blob: Arc<crate::persistence::blob_store::DiskBlob>,
        ttl: Option<Duration>,
    ) -> Self {
        let now = Instant::now();
        Self {
            value: KvValue::Null,
            spilled: Some(blob),
            created_at: now,
            expires_at: ttl.map(|d| now + d),
            version: 1,
            ram_bytes: 0,
            last_accessed: AtomicU64::new(now_access_stamp()),
            access_count: AtomicU64::new(1),
        }
    }

    /// Construct an entry with explicit timestamps and version (used by snapshot restore)
    pub fn with_timestamps(
        value: KvValue,
        created_at: Instant,
        expires_at: Option<Instant>,
        version: u64,
    ) -> Self {
        Self {
            value,
            spilled: None,
            created_at,
            expires_at,
            version,
            ram_bytes: 0,
            last_accessed: AtomicU64::new(now_access_stamp()),
            access_count: AtomicU64::new(1),
        }
    }

    /// Check if entry has expired
    #[inline]
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|exp| Instant::now() >= exp)
            .unwrap_or(false)
    }

    /// Record an access lock-free (safe to call under a shared `RwLock::read` guard)
    #[inline]
    pub fn touch(&self) {
        self.last_accessed
            .store(now_access_stamp(), Ordering::Relaxed);
        self.access_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Materialize the value, reading from disk if spilled
    pub fn materialize_value(&self) -> Option<KvValue> {
        if let Some(blob) = &self.spilled {
            blob.read_value().ok()
        } else {
            Some(self.value.clone())
        }
    }

    /// Consume a cloned entry and populate `self.value` from disk if spilled
    pub fn materialize_into(mut self) -> Option<Self> {
        if let Some(blob) = &self.spilled {
            self.value = blob.read_value().ok()?;
        }
        Some(self)
    }
}

/// Candidate information returned by shard eviction sampling
#[derive(Debug, Clone)]
pub struct EvictionCandidate {
    pub key: String,
    pub last_accessed: u64,
    pub access_count: u64,
    pub can_spill: bool,
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

    // ==================== Memory & Eviction ====================

    /// O(1) resident memory usage for this shard
    #[inline]
    pub fn estimate_memory(&self) -> usize {
        self.used_memory_bytes.load(Ordering::Relaxed)
    }

    /// Estimate size of a KvValue in RAM
    pub fn estimate_value_size(value: &KvValue) -> usize {
        match value {
            KvValue::Int(_) => 8,
            KvValue::Float(_) => 8,
            KvValue::Decimal(_) => 16,
            KvValue::String(s) => s.len() + 24,
            KvValue::Bytes(b) => b.len() + 24,
            KvValue::List(l) => l.iter().map(Self::estimate_value_size).sum::<usize>() + 24,
            KvValue::Map(m) => {
                m.iter()
                    .map(|(k, v)| k.len() + Self::estimate_value_size(v))
                    .sum::<usize>()
                    + 48
            }
            KvValue::Set(s) => s.iter().map(|m| m.len()).sum::<usize>() + 48,
            KvValue::SortedSet(ss) => ss.iter().map(|(m, _)| m.len() + 8).sum::<usize>() + 48,
            KvValue::Null => 0,
        }
    }

    /// Sample up to `sample_limit` entries from this shard and return the best
    /// eviction candidate according to `use_lfu` and `volatile_only`.
    pub fn sample_eviction_candidate(
        &self,
        volatile_only: bool,
        use_lfu: bool,
        sample_limit: usize,
    ) -> Option<EvictionCandidate> {
        let guard = self.data.read();
        if guard.is_empty() {
            return None;
        }

        let mut best: Option<EvictionCandidate> = None;

        for (key, entry) in guard.iter().take(sample_limit) {
            if entry.is_expired() {
                continue;
            }
            if volatile_only && entry.expires_at.is_none() {
                continue;
            }

            let last_accessed = entry.last_accessed.load(Ordering::Relaxed);
            let access_count = entry.access_count.load(Ordering::Relaxed);
            let can_spill = entry.spilled.is_none()
                && matches!(
                    &entry.value,
                    KvValue::Bytes(b) if b.len() >= 128
                )
                || (entry.spilled.is_none()
                    && matches!(
                        &entry.value,
                        KvValue::String(s) if s.len() >= 128
                    ));

            let candidate = EvictionCandidate {
                key: key.clone(),
                last_accessed,
                access_count,
                can_spill,
            };

            match &best {
                None => best = Some(candidate),
                Some(cur) => {
                    let is_better = if use_lfu {
                        (candidate.access_count, candidate.last_accessed)
                            < (cur.access_count, cur.last_accessed)
                    } else {
                        candidate.last_accessed < cur.last_accessed
                    };
                    if is_better {
                        best = Some(candidate);
                    }
                }
            }
        }

        best
    }

    /// Get the least recently used key (for LRU eviction)
    pub fn get_lru_key(&self, volatile_only: bool) -> Option<String> {
        self.sample_eviction_candidate(volatile_only, false, usize::MAX)
            .map(|c| c.key)
    }

    /// Get the least frequently used key (for LFU eviction)
    pub fn get_lfu_key(&self, volatile_only: bool) -> Option<String> {
        self.sample_eviction_candidate(volatile_only, true, usize::MAX)
            .map(|c| c.key)
    }

    /// Attempt to spill an in-memory `Bytes` or `String` entry to `blob_store`,
    /// freeing its RAM while keeping the key alive.
    pub fn try_spill_key(
        &self,
        key: &str,
        blob_store: &crate::persistence::blob_store::BlobStore,
    ) -> bool {
        let value_to_spill = {
            let guard = self.data.read();
            match guard.get(key) {
                Some(entry) if !entry.is_expired() && entry.spilled.is_none() => match &entry.value
                {
                    KvValue::Bytes(_) | KvValue::String(_) => {
                        Some((entry.value.clone(), entry.version))
                    }
                    _ => None,
                },
                _ => None,
            }
        };

        let Some((val, expected_version)) = value_to_spill else {
            return false;
        };

        let Ok(Some(blob)) = blob_store.store_value(&val) else {
            return false;
        };

        let mut guard = self.data.write();
        if let Some(entry) = guard.get_mut(key) {
            if !entry.is_expired() && entry.version == expected_version && entry.spilled.is_none() {
                entry.value = KvValue::Null;
                entry.spilled = Some(blob);
                self.refresh_entry_ram(key.len(), entry);
                return true;
            }
        }
        false
    }

    /// Export all entries (for persistence/snapshots), materializing any spilled blobs
    pub fn export_all(&self) -> HashMap<String, Entry> {
        let cloned = {
            let guard = self.data.read();
            guard.clone()
        };
        cloned
            .into_iter()
            .filter_map(|(k, e)| e.materialize_into().map(|mat| (k, mat)))
            .collect()
    }

    /// Import entries (for recovery from snapshots)
    pub fn import_all(&self, entries: HashMap<String, Entry>) {
        let mut guard = self.data.write();
        for (key, entry) in entries {
            if !entry.is_expired() {
                self.insert_entry(&mut guard, key, entry);
            }
        }
    }
}

impl Default for Shard {
    fn default() -> Self {
        Self::new()
    }
}

/// Memory eviction policy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// Evict least recently used keys (any key)
    AllKeysLru,
    /// Evict least recently used keys (only keys with TTL)
    VolatileLru,
    /// Evict least frequently used keys (any key)
    AllKeysLfu,
    /// Don't evict, return OOM error
    NoEviction,
}

impl EvictionPolicy {
    /// Parse eviction policy from string (`allkeys-lru`, `volatile-lru`, `allkeys-lfu`, `noeviction`)
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allkeys-lru" | "lru" => Some(Self::AllKeysLru),
            "volatile-lru" => Some(Self::VolatileLru),
            "allkeys-lfu" | "lfu" => Some(Self::AllKeysLfu),
            "noeviction" | "none" => Some(Self::NoEviction),
            _ => None,
        }
    }

    /// Canonical policy name
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AllKeysLru => "allkeys-lru",
            Self::VolatileLru => "volatile-lru",
            Self::AllKeysLfu => "allkeys-lfu",
            Self::NoEviction => "noeviction",
        }
    }
}

impl Default for EvictionPolicy {
    fn default() -> Self {
        Self::AllKeysLru
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

    /// Enable disk blob tiering (`/data/blobs/`) on this engine
    pub fn enable_blob_store(&self, store: Arc<crate::persistence::blob_store::BlobStore>) {
        self.blob_store.store(Some(store));
        self.blob_store_enabled.store(true, Ordering::Release);
    }

    /// Return current disk bytes used by tiered blobs (0 if disabled)
    pub fn used_disk_bytes(&self) -> usize {
        if !self.blob_store_enabled.load(Ordering::Acquire) {
            return 0;
        }
        self.blob_store
            .load()
            .as_ref()
            .map(|b| b.used_disk_bytes())
            .unwrap_or(0)
    }

    /// Return total number of keys evicted
    pub fn evicted_keys(&self) -> u64 {
        self.evicted_keys.load(Ordering::Relaxed)
    }

    /// Return total number of keys spilled to disk
    pub fn spilled_keys(&self) -> u64 {
        self.spilled_keys.load(Ordering::Relaxed)
    }

    /// Set maximum memory limit (bytes). 0 = unlimited.
    pub fn set_maxmemory(&self, bytes: usize) {
        self.maxmemory.store(bytes, Ordering::Relaxed);
    }

    /// Get current maximum memory limit
    pub fn get_maxmemory(&self) -> usize {
        self.maxmemory.load(Ordering::Relaxed)
    }

    /// Set eviction policy
    pub fn set_eviction_policy(&self, policy: EvictionPolicy) {
        *self.eviction_policy.write() = policy;
    }

    /// Get current eviction policy
    pub fn get_eviction_policy(&self) -> EvictionPolicy {
        *self.eviction_policy.read()
    }

    /// O(1) total resident RAM usage across all shards
    pub fn estimate_memory(&self) -> usize {
        self.shards.iter().map(|s| s.estimate_memory()).sum()
    }

    /// Build an `Entry` for `value`, automatically spilling to `/data/blobs/` if
    /// `value` exceeds `spill_threshold_bytes`.
    #[inline]
    fn prepare_entry(&self, value: KvValue, ttl: Option<Duration>) -> Entry {
        if self.blob_store_enabled.load(Ordering::Acquire) {
            if let Some(store) = &*self.blob_store.load() {
                if store.should_spill_on_write(&value) {
                    if let Ok(Some(blob)) = store.store_value(&value) {
                        self.spilled_keys.fetch_add(1, Ordering::Relaxed);
                        return Entry::new_spilled(blob, ttl);
                    }
                }
            }
        }
        Entry::new(value, ttl)
    }

    /// Check if memory limit is exceeded and try to spill cold blobs or evict keys.
    /// Returns Ok(()) if within limit or eviction succeeded, Err if OOM.
    fn check_memory_and_evict(&self) -> Result<(), KvError> {
        let maxmem = self.maxmemory.load(Ordering::Relaxed);
        if maxmem == 0 {
            return Ok(());
        }

        if self.estimate_memory() < maxmem {
            return Ok(());
        }

        let policy = *self.eviction_policy.read();
        if policy == EvictionPolicy::NoEviction {
            return Err(KvError::OutOfMemory);
        }

        let volatile_only = matches!(policy, EvictionPolicy::VolatileLru);
        let use_lfu = matches!(policy, EvictionPolicy::AllKeysLfu);
        let blob_store = if self.blob_store_enabled.load(Ordering::Acquire) {
            self.blob_store.load_full()
        } else {
            None
        };

        for _ in 0..16 {
            let Some(candidate) = self.find_eviction_candidate(volatile_only, use_lfu) else {
                return Err(KvError::OutOfMemory);
            };

            // Spill-before-evict: if blob store is enabled and candidate has a
            // spillable in-memory payload, spill it to disk to free RAM without losing data.
            let mut spilled = false;
            if candidate.can_spill {
                if let Some(ref store) = blob_store {
                    if self
                        .shard_for_key(&candidate.key)
                        .try_spill_key(&candidate.key, store)
                    {
                        self.spilled_keys.fetch_add(1, Ordering::Relaxed);
                        spilled = true;
                    }
                }
            }

            if !spilled {
                if let Ok(kv_key) = KvKey::new(candidate.key) {
                    self.delete(&kv_key);
                    self.evicted_keys.fetch_add(1, Ordering::Relaxed);
                }
            }

            if self.estimate_memory() < maxmem {
                return Ok(());
            }
        }

        Err(KvError::OutOfMemory)
    }

    /// Find a key to evict based on approximated sampling across shards
    fn find_eviction_candidate(
        &self,
        volatile_only: bool,
        use_lfu: bool,
    ) -> Option<EvictionCandidate> {
        const MAX_NON_EMPTY_SHARDS_TO_PROBE: usize = 16;
        const KEYS_PER_SHARD_SAMPLE: usize = 8;

        let start_idx = self.eviction_cursor.fetch_add(1, Ordering::Relaxed) % self.num_shards;
        let mut probed_non_empty = 0usize;
        let mut best: Option<EvictionCandidate> = None;

        for offset in 0..self.num_shards {
            let shard_idx = (start_idx + offset) % self.num_shards;
            let shard = &self.shards[shard_idx];

            if let Some(candidate) =
                shard.sample_eviction_candidate(volatile_only, use_lfu, KEYS_PER_SHARD_SAMPLE)
            {
                probed_non_empty += 1;
                match &best {
                    None => best = Some(candidate),
                    Some(cur) => {
                        let is_better = if use_lfu {
                            (candidate.access_count, candidate.last_accessed)
                                < (cur.access_count, cur.last_accessed)
                        } else {
                            candidate.last_accessed < cur.last_accessed
                        };
                        if is_better {
                            best = Some(candidate);
                        }
                    }
                }
                if probed_non_empty >= MAX_NON_EMPTY_SHARDS_TO_PROBE {
                    break;
                }
            }
        }

        best
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

    // ==================== Batch Operations ====================

    /// Get multiple values by keys (MGET)
    pub fn mget(&self, keys: &[&KvKey]) -> Vec<Option<KvValue>> {
        keys.iter().map(|key| self.get(key)).collect()
    }

    /// Set multiple key-value pairs (MSET)
    pub fn mset(&self, pairs: &[(&KvKey, KvValue)], ttl: Option<Duration>) -> Result<(), KvError> {
        self.check_memory_and_evict()?;

        let wal_pairs: Vec<(String, KvValue)> = pairs
            .iter()
            .map(|(key, value)| (key.as_str().to_string(), value.clone()))
            .collect();

        self.log_wal(crate::persistence::format::WalOp::MSet {
            pairs: wal_pairs,
            ttl,
        });

        for (key, value) in pairs {
            let entry = self.prepare_entry(value.clone(), ttl);
            self.shard_for_key(key.as_str())
                .set_entry(key.as_str().to_string(), entry);
        }
        Ok(())
    }

    /// Delete multiple keys (MDEL)
    ///
    /// Deletes multiple keys in a single operation.
    ///
    /// # Returns
    /// The number of keys that were actually deleted (existed before deletion).
    ///
    /// # Example
    /// ```
    /// use keep_keyspace::engine::KvEngine;
    /// use keep_keyspace::types::{KvKey, KvValue};
    ///
    /// let engine = KvEngine::new();
    /// let key1 = KvKey::new("key1").unwrap();
    /// let key2 = KvKey::new("key2").unwrap();
    /// let key3 = KvKey::new("key3").unwrap();
    ///
    /// engine.set(&key1, KvValue::Int(1), None);
    /// engine.set(&key2, KvValue::Int(2), None);
    ///
    /// let keys = vec![&key1, &key2, &key3];
    /// let deleted = engine.mdel(&keys);
    /// assert_eq!(deleted, 2); // key1 and key2 deleted, key3 didn't exist
    /// ```
    pub fn mdel(&self, keys: &[&KvKey]) -> usize {
        // Log single batch operation to WAL
        let wal_keys: Vec<String> = keys.iter().map(|key| key.as_str().to_string()).collect();

        self.log_wal(crate::persistence::format::WalOp::MDel { keys: wal_keys });

        // Apply to in-memory shards
        keys.iter()
            .filter(|key| {
                self.shard_for_key(key.as_str())
                    .delete(key.as_str())
                    .is_some()
            })
            .count()
    }

    /// Check if multiple keys exist (MEXISTS)
    ///
    /// Returns a vector of booleans indicating whether each key exists.
    ///
    /// # Example
    /// ```
    /// use keep_keyspace::engine::KvEngine;
    /// use keep_keyspace::types::{KvKey, KvValue};
    ///
    /// let engine = KvEngine::new();
    /// let key1 = KvKey::new("key1").unwrap();
    /// let key2 = KvKey::new("key2").unwrap();
    ///
    /// engine.set(&key1, KvValue::Int(1), None);
    ///
    /// let keys = vec![&key1, &key2];
    /// let exists = engine.mexists(&keys);
    /// assert_eq!(exists, vec![true, false]);
    /// ```
    pub fn mexists(&self, keys: &[&KvKey]) -> Vec<bool> {
        keys.iter().map(|key| self.exists(key)).collect()
    }

    // ==================== TTL Management ====================

    /// Set TTL for a key (EXPIRE - seconds)
    /// Returns 1 if TTL was set, 0 if key doesn't exist
    pub fn expire(&self, key: &KvKey, seconds: i64) -> i32 {
        if seconds <= 0 {
            // TTL of 0 or negative = delete immediately (delete logs its own WAL)
            return if self.delete(key) { 1 } else { 0 };
        }
        self.log_wal(crate::persistence::format::WalOp::Expire {
            key: key.as_str().to_string(),
            seconds,
        });
        self.shard_for_key(key.as_str())
            .expire(key.as_str(), Duration::from_secs(seconds as u64))
    }

    /// Set TTL for a key (PEXPIRE - milliseconds)
    /// Returns 1 if TTL was set, 0 if key doesn't exist
    pub fn pexpire(&self, key: &KvKey, milliseconds: i64) -> i32 {
        if milliseconds <= 0 {
            return if self.delete(key) { 1 } else { 0 };
        }
        self.log_wal(crate::persistence::format::WalOp::PExpire {
            key: key.as_str().to_string(),
            milliseconds,
        });
        self.shard_for_key(key.as_str())
            .expire(key.as_str(), Duration::from_millis(milliseconds as u64))
    }

    /// Get remaining TTL in seconds
    /// Returns -2 if key doesn't exist, -1 if no TTL, or remaining seconds
    pub fn ttl(&self, key: &KvKey) -> i64 {
        self.shard_for_key(key.as_str()).ttl(key.as_str())
    }

    /// Get remaining TTL in milliseconds
    /// Returns -2 if key doesn't exist, -1 if no TTL, or remaining milliseconds
    pub fn pttl(&self, key: &KvKey) -> i64 {
        self.shard_for_key(key.as_str()).pttl(key.as_str())
    }

    /// Remove TTL from a key (PERSIST)
    /// Returns 1 if TTL was removed, 0 if key doesn't exist or has no TTL
    pub fn persist(&self, key: &KvKey) -> i32 {
        self.log_wal(crate::persistence::format::WalOp::Persist {
            key: key.as_str().to_string(),
        });
        self.shard_for_key(key.as_str()).persist(key.as_str())
    }

    /// Get value and optionally update TTL (GETEX)
    /// Options: EX seconds, PX milliseconds, EXAT unix-time-seconds,
    ///          PXAT unix-time-milliseconds, PERSIST
    pub fn getex(&self, key: &KvKey, ttl: Option<Duration>, persist: bool) -> Option<KvValue> {
        // Only a TTL-changing GETEX mutates state worth logging; a plain GETEX
        // (no ttl, no persist) is a pure read.
        if ttl.is_some() || persist {
            self.log_wal(crate::persistence::format::WalOp::GetEx {
                key: key.as_str().to_string(),
                ttl_ms: ttl.map(|d| d.as_millis() as u64),
                persist,
            });
        }
        self.shard_for_key(key.as_str())
            .getex(key.as_str(), ttl, persist)
            .map(|e| e.value)
    }

    // ==================== Hash Operations ====================

    /// Set fields in a hash (HSET/HMSET)
    /// Creates the hash if it doesn't exist
    /// Returns number of new fields added (not updated)
    pub fn hset(&self, key: &KvKey, fields: Vec<(String, KvValue)>) -> Result<usize, KvError> {
        self.check_memory_and_evict()?;
        self.log_wal(crate::persistence::format::WalOp::HSet {
            key: key.as_str().to_string(),
            fields: fields.clone(),
        });
        self.shard_for_key(key.as_str()).hset(key.as_str(), fields)
    }

    /// Get a field from a hash (HGET)
    /// Returns WRONGTYPE error if key holds wrong type
    pub fn hget(&self, key: &KvKey, field: &str) -> Result<Option<KvValue>, KvError> {
        self.shard_for_key(key.as_str()).hget(key.as_str(), field)
    }

    /// Get multiple fields from a hash (HMGET)
    /// Returns WRONGTYPE error if key holds wrong type
    pub fn hmget(&self, key: &KvKey, fields: &[&str]) -> Result<Vec<Option<KvValue>>, KvError> {
        self.shard_for_key(key.as_str()).hmget(key.as_str(), fields)
    }

    /// Get all fields and values from a hash (HGETALL)
    /// Returns empty HashMap for non-existent keys
    /// Returns WRONGTYPE error if key holds wrong type
    pub fn hgetall(
        &self,
        key: &KvKey,
    ) -> Result<std::collections::HashMap<String, KvValue>, KvError> {
        self.shard_for_key(key.as_str()).hgetall(key.as_str())
    }

    /// Delete fields from a hash (HDEL)
    /// Returns number of fields removed
    pub fn hdel(&self, key: &KvKey, fields: &[&str]) -> Result<usize, KvError> {
        self.log_wal(crate::persistence::format::WalOp::HDel {
            key: key.as_str().to_string(),
            fields: fields.iter().map(|f| f.to_string()).collect(),
        });
        self.shard_for_key(key.as_str()).hdel(key.as_str(), fields)
    }

    /// Check if a field exists in a hash (HEXISTS)
    /// Returns WRONGTYPE error if key holds wrong type
    pub fn hexists(&self, key: &KvKey, field: &str) -> Result<bool, KvError> {
        self.shard_for_key(key.as_str())
            .hexists(key.as_str(), field)
    }

    /// Get number of fields in a hash (HLEN)
    /// Returns WRONGTYPE error if key holds wrong type
    pub fn hlen(&self, key: &KvKey) -> Result<usize, KvError> {
        self.shard_for_key(key.as_str()).hlen(key.as_str())
    }

    /// Increment integer value of a hash field (HINCRBY)
    /// Creates hash/field if not exists. Returns new value.
    pub fn hincrby(&self, key: &KvKey, field: &str, increment: i64) -> Result<i64, KvError> {
        self.check_memory_and_evict()?;
        self.log_wal(crate::persistence::format::WalOp::HIncrBy {
            key: key.as_str().to_string(),
            field: field.to_string(),
            delta: increment,
        });
        self.shard_for_key(key.as_str())
            .hincrby(key.as_str(), field, increment)
    }

    /// Increment float value of a hash field (HINCRBYFLOAT)
    /// Creates hash/field if not exists. Returns new value.
    pub fn hincrbyfloat(&self, key: &KvKey, field: &str, increment: f64) -> Result<f64, KvError> {
        self.check_memory_and_evict()?;
        self.log_wal(crate::persistence::format::WalOp::HIncrByFloat {
            key: key.as_str().to_string(),
            field: field.to_string(),
            delta: increment,
        });
        self.shard_for_key(key.as_str())
            .hincrbyfloat(key.as_str(), field, increment)
    }

    // ==================== List Operations ====================

    /// Push elements to head of list (LPUSH)
    /// Creates the list if it doesn't exist
    /// Returns new length of list
    pub fn lpush(&self, key: &KvKey, values: Vec<KvValue>) -> Result<usize, KvError> {
        self.check_memory_and_evict()?;
        self.log_wal(crate::persistence::format::WalOp::LPush {
            key: key.as_str().to_string(),
            values: values.clone(),
        });
        self.shard_for_key(key.as_str()).lpush(key.as_str(), values)
    }

    /// Push elements to tail of list (RPUSH)
    /// Creates the list if it doesn't exist
    /// Returns new length of list
    pub fn rpush(&self, key: &KvKey, values: Vec<KvValue>) -> Result<usize, KvError> {
        self.check_memory_and_evict()?;
        self.log_wal(crate::persistence::format::WalOp::RPush {
            key: key.as_str().to_string(),
            values: values.clone(),
        });
        self.shard_for_key(key.as_str()).rpush(key.as_str(), values)
    }

    /// Pop element from head of list (LPOP)
    pub fn lpop(&self, key: &KvKey) -> Option<KvValue> {
        self.log_wal(crate::persistence::format::WalOp::LPop {
            key: key.as_str().to_string(),
        });
        self.shard_for_key(key.as_str()).lpop(key.as_str())
    }

    /// Pop element from tail of list (RPOP)
    pub fn rpop(&self, key: &KvKey) -> Option<KvValue> {
        self.log_wal(crate::persistence::format::WalOp::RPop {
            key: key.as_str().to_string(),
        });
        self.shard_for_key(key.as_str()).rpop(key.as_str())
    }

    /// Get range of elements from list (LRANGE)
    /// Supports negative indices (-1 = last element)
    /// Returns WRONGTYPE error if key holds wrong type
    pub fn lrange(&self, key: &KvKey, start: i64, stop: i64) -> Result<Vec<KvValue>, KvError> {
        self.shard_for_key(key.as_str())
            .lrange(key.as_str(), start, stop)
    }

    /// Get length of list (LLEN)
    /// Returns WRONGTYPE error if key holds wrong type
    pub fn llen(&self, key: &KvKey) -> Result<usize, KvError> {
        self.shard_for_key(key.as_str()).llen(key.as_str())
    }

    // ==================== Set Operations ====================
    // Engine-level wrappers over the per-shard set ops. Like the hash/list
    // collection ops, these are in-memory only (the WAL covers scalar ops only).

    /// Add members to a set (SADD). Returns the number newly added.
    pub fn sadd(&self, key: &KvKey, members: Vec<String>) -> Result<usize, KvError> {
        self.check_memory_and_evict()?;
        self.log_wal(crate::persistence::format::WalOp::SAdd {
            key: key.as_str().to_string(),
            members: members.clone(),
        });
        self.shard_for_key(key.as_str()).sadd(key.as_str(), members)
    }

    /// Remove members from a set (SREM). Returns the number removed.
    pub fn srem(&self, key: &KvKey, members: Vec<String>) -> Result<usize, KvError> {
        self.log_wal(crate::persistence::format::WalOp::SRem {
            key: key.as_str().to_string(),
            members: members.clone(),
        });
        self.shard_for_key(key.as_str()).srem(key.as_str(), members)
    }

    /// All members of a set (SMEMBERS).
    pub fn smembers(&self, key: &KvKey) -> Result<Vec<String>, KvError> {
        self.shard_for_key(key.as_str()).smembers(key.as_str())
    }

    /// Membership test (SISMEMBER).
    pub fn sismember(&self, key: &KvKey, member: &str) -> Result<bool, KvError> {
        self.shard_for_key(key.as_str())
            .sismember(key.as_str(), member)
    }

    /// Set cardinality (SCARD).
    pub fn scard(&self, key: &KvKey) -> Result<usize, KvError> {
        self.shard_for_key(key.as_str()).scard(key.as_str())
    }

    // ==================== Sorted-Set Operations ====================

    /// Add scored members to a sorted set (ZADD). Returns the number newly added.
    pub fn zadd(&self, key: &KvKey, members: Vec<(String, f64)>) -> Result<usize, KvError> {
        self.check_memory_and_evict()?;
        self.log_wal(crate::persistence::format::WalOp::ZAdd {
            key: key.as_str().to_string(),
            members: members.clone(),
        });
        self.shard_for_key(key.as_str()).zadd(key.as_str(), members)
    }

    /// Remove members from a sorted set (ZREM). Returns the number removed.
    pub fn zrem(&self, key: &KvKey, members: Vec<String>) -> Result<usize, KvError> {
        self.log_wal(crate::persistence::format::WalOp::ZRem {
            key: key.as_str().to_string(),
            members: members.clone(),
        });
        self.shard_for_key(key.as_str()).zrem(key.as_str(), members)
    }

    /// Score of a member (ZSCORE).
    pub fn zscore(&self, key: &KvKey, member: &str) -> Result<Option<f64>, KvError> {
        self.shard_for_key(key.as_str())
            .zscore(key.as_str(), member)
    }

    /// Increment a member's score (ZINCRBY). Returns the new score.
    pub fn zincrby(&self, key: &KvKey, member: &str, increment: f64) -> Result<f64, KvError> {
        self.check_memory_and_evict()?;
        self.log_wal(crate::persistence::format::WalOp::ZIncrBy {
            key: key.as_str().to_string(),
            member: member.to_string(),
            delta: increment,
        });
        self.shard_for_key(key.as_str())
            .zincrby(key.as_str(), member, increment)
    }

    /// Rank (0-based, ascending by score) of a member (ZRANK).
    pub fn zrank(&self, key: &KvKey, member: &str) -> Result<Option<usize>, KvError> {
        self.shard_for_key(key.as_str()).zrank(key.as_str(), member)
    }

    /// Members in a rank range, ascending by score (ZRANGE). Negative indices
    /// count from the end.
    pub fn zrange(
        &self,
        key: &KvKey,
        start: i64,
        stop: i64,
    ) -> Result<Vec<(String, f64)>, KvError> {
        self.shard_for_key(key.as_str())
            .zrange(key.as_str(), start, stop)
    }

    /// Sorted-set cardinality (ZCARD).
    pub fn zcard(&self, key: &KvKey) -> Result<usize, KvError> {
        self.shard_for_key(key.as_str()).zcard(key.as_str())
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
