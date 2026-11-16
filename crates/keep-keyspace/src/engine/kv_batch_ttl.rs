use super::*;

impl KvEngine {
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
}
