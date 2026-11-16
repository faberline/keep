use super::*;

impl KvEngine {
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
}
