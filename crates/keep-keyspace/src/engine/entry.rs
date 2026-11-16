use super::*;

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
