use super::*;

impl Shard {
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

impl KvEngine {
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
    pub(super) fn prepare_entry(&self, value: KvValue, ttl: Option<Duration>) -> Entry {
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
    pub(super) fn check_memory_and_evict(&self) -> Result<(), KvError> {
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
}
