use super::*;
use rust_decimal::Decimal;
use std::thread;

#[test]
fn test_basic_set_get() {
    let engine = KvEngine::new();
    let key = KvKey::new("test_key").unwrap();

    engine
        .set(&key, KvValue::String("hello".to_string()), None)
        .unwrap();

    let result = engine.get(&key);
    assert_eq!(result, Some(KvValue::String("hello".to_string())));
}

#[test]
fn test_get_nonexistent() {
    let engine = KvEngine::new();
    let key = KvKey::new("nonexistent").unwrap();

    assert_eq!(engine.get(&key), None);
}

#[test]
fn test_delete() {
    let engine = KvEngine::new();
    let key = KvKey::new("to_delete").unwrap();

    engine.set(&key, KvValue::Int(42), None).unwrap();
    assert!(engine.exists(&key));

    assert!(engine.delete(&key));
    assert!(!engine.exists(&key));
}

#[test]
fn test_exists() {
    let engine = KvEngine::new();
    let key = KvKey::new("exists_key").unwrap();

    assert!(!engine.exists(&key));
    engine.set(&key, KvValue::Int(1), None).unwrap();
    assert!(engine.exists(&key));
}

#[test]
fn test_incr_existing() {
    let engine = KvEngine::new();
    let key = KvKey::new("counter").unwrap();

    engine.set(&key, KvValue::Int(10), None).unwrap();

    let result = engine.incr(&key, 5).unwrap();
    assert_eq!(result, 15);

    let result = engine.decr(&key, 3).unwrap();
    assert_eq!(result, 12);
}

#[test]
fn test_incr_nonexistent() {
    let engine = KvEngine::new();
    let key = KvKey::new("new_counter").unwrap();

    let result = engine.incr(&key, 100).unwrap();
    assert_eq!(result, 100);
}

#[test]
fn test_incr_type_mismatch() {
    let engine = KvEngine::new();
    let key = KvKey::new("string_key").unwrap();

    engine
        .set(&key, KvValue::String("not a number".to_string()), None)
        .unwrap();

    let result = engine.incr(&key, 1);
    assert!(matches!(result, Err(KvError::TypeMismatch { .. })));
}

#[test]
fn test_cas_success() {
    let engine = KvEngine::new();
    let key = KvKey::new("cas_key").unwrap();

    engine
        .set(&key, KvValue::String("initial".to_string()), None)
        .unwrap();

    let result = engine
        .cas(
            &key,
            &KvValue::String("initial".to_string()),
            KvValue::String("updated".to_string()),
            None,
        )
        .unwrap();

    assert!(result);
    assert_eq!(
        engine.get(&key),
        Some(KvValue::String("updated".to_string()))
    );
}

#[test]
fn test_cas_failure() {
    let engine = KvEngine::new();
    let key = KvKey::new("cas_key").unwrap();

    engine
        .set(&key, KvValue::String("actual".to_string()), None)
        .unwrap();

    let result = engine
        .cas(
            &key,
            &KvValue::String("wrong_expected".to_string()),
            KvValue::String("new".to_string()),
            None,
        )
        .unwrap();

    assert!(!result);
    assert_eq!(
        engine.get(&key),
        Some(KvValue::String("actual".to_string()))
    );
}

#[test]
fn test_cas_nonexistent() {
    let engine = KvEngine::new();
    let key = KvKey::new("missing").unwrap();

    let result = engine.cas(&key, &KvValue::Int(0), KvValue::Int(1), None);

    assert!(matches!(result, Err(KvError::KeyNotFound(_))));
}

#[test]
fn test_ttl_expiration() {
    let engine = KvEngine::new();
    let key = KvKey::new("ttl_key").unwrap();

    // Set with 10ms TTL
    engine
        .set(&key, KvValue::Int(42), Some(Duration::from_millis(10)))
        .unwrap();

    // Should exist immediately
    assert!(engine.exists(&key));

    // Wait for expiration
    thread::sleep(Duration::from_millis(20));

    // Should be gone
    assert!(!engine.exists(&key));
    assert_eq!(engine.get(&key), None);
}

#[test]
fn test_decimal_value() {
    let engine = KvEngine::new();
    let key = KvKey::new("decimal_key").unwrap();

    let decimal = Decimal::new(12345, 2); // 123.45
    engine.set(&key, KvValue::Decimal(decimal), None).unwrap();

    let result = engine.get(&key);
    assert_eq!(result, Some(KvValue::Decimal(decimal)));
}

#[test]
fn test_concurrent_access() {
    use std::sync::Arc;

    let engine = Arc::new(KvEngine::new());
    let mut handles = vec![];

    // Spawn 10 threads, each incrementing 100 different keys
    for _t in 0..10 {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || {
            for i in 0..100 {
                let key = KvKey::new(format!("key_{}", i)).unwrap();
                engine.incr(&key, 1).unwrap();
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Each of 100 keys should have been incremented 10 times
    for i in 0..100 {
        let key = KvKey::new(format!("key_{}", i)).unwrap();
        assert_eq!(engine.get(&key), Some(KvValue::Int(10)));
    }
}

#[test]
fn test_sharding_distribution() {
    let engine = KvEngine::with_shards(16);

    // Insert 1000 keys
    for i in 0..1000 {
        let key = KvKey::new(format!("key_{}", i)).unwrap();
        engine.set(&key, KvValue::Int(i), None).unwrap();
    }

    // Check that keys are distributed across shards
    let mut non_empty_shards = 0;
    for shard in &engine.shards {
        if !shard.is_empty() {
            non_empty_shards += 1;
        }
    }

    // With 1000 keys and 16 shards, we expect most shards to have keys
    assert!(
        non_empty_shards >= 14,
        "Poor distribution: only {} shards have data",
        non_empty_shards
    );
}

#[test]
fn test_cleanup_expired() {
    let engine = KvEngine::new();

    // Add some keys with short TTL
    for i in 0..10 {
        let key = KvKey::new(format!("expire_{}", i)).unwrap();
        engine
            .set(&key, KvValue::Int(i), Some(Duration::from_millis(5)))
            .unwrap();
    }

    // Add some keys without TTL
    for i in 0..10 {
        let key = KvKey::new(format!("persist_{}", i)).unwrap();
        engine.set(&key, KvValue::Int(i), None).unwrap();
    }

    assert_eq!(engine.len(), 20);

    // Wait for expiration
    thread::sleep(Duration::from_millis(10));

    // Cleanup
    let removed = engine.cleanup_expired();
    assert_eq!(removed, 10);
    assert_eq!(engine.len(), 10);
}

#[test]
fn test_setnx_success() {
    let engine = KvEngine::new();
    let key = KvKey::new("setnx_key").unwrap();

    // First SETNX should succeed
    assert!(engine
        .setnx(&key, KvValue::String("value1".to_string()), None)
        .unwrap());

    // Second SETNX should fail (key exists)
    assert!(!engine
        .setnx(&key, KvValue::String("value2".to_string()), None)
        .unwrap());

    // Value should still be the first one
    assert_eq!(
        engine.get(&key),
        Some(KvValue::String("value1".to_string()))
    );
}

#[test]
fn test_setnx_expired() {
    let engine = KvEngine::new();
    let key = KvKey::new("setnx_expired").unwrap();

    // Set with short TTL
    engine
        .setnx(
            &key,
            KvValue::String("old".to_string()),
            Some(Duration::from_millis(10)),
        )
        .unwrap();

    // Wait for expiration
    thread::sleep(Duration::from_millis(20));

    // SETNX should now succeed (expired)
    assert!(engine
        .setnx(&key, KvValue::String("new".to_string()), None)
        .unwrap());
    assert_eq!(engine.get(&key), Some(KvValue::String("new".to_string())));
}

#[test]
fn test_lock_unlock() {
    let engine = KvEngine::new();
    let key = KvKey::new("lock_test").unwrap();

    // Acquire lock
    assert!(engine.lock(&key, "worker-1", Duration::from_secs(30)));

    // Second acquire should fail
    assert!(!engine.lock(&key, "worker-2", Duration::from_secs(30)));

    // Unlock by wrong owner should fail
    let result = engine.unlock(&key, "worker-2");
    assert!(matches!(result, Err(KvError::LockOwnerMismatch { .. })));

    // Unlock by correct owner should succeed
    assert!(engine.unlock(&key, "worker-1").unwrap());

    // Now another worker can acquire
    assert!(engine.lock(&key, "worker-2", Duration::from_secs(30)));
}

#[test]
fn test_lock_expiration() {
    let engine = KvEngine::new();
    let key = KvKey::new("lock_expire").unwrap();

    // Acquire with short TTL
    assert!(engine.lock(&key, "worker-1", Duration::from_millis(10)));

    // Wait for expiration
    thread::sleep(Duration::from_millis(20));

    // Another worker can now acquire (lock expired)
    assert!(engine.lock(&key, "worker-2", Duration::from_secs(30)));
}

#[test]
fn test_extend_lock() {
    let engine = KvEngine::new();
    let key = KvKey::new("lock_extend").unwrap();

    // Acquire lock
    assert!(engine.lock(&key, "worker-1", Duration::from_millis(50)));

    // Extend by wrong owner should fail
    let result = engine.extend_lock(&key, "worker-2", Duration::from_secs(30));
    assert!(matches!(result, Err(KvError::LockOwnerMismatch { .. })));

    // Extend by correct owner should succeed
    assert!(engine
        .extend_lock(&key, "worker-1", Duration::from_secs(30))
        .unwrap());

    // Wait a bit - lock should still be held (was extended)
    thread::sleep(Duration::from_millis(60));
    assert!(engine.exists(&key));
}

// ==================== TTL Management Tests ====================

#[test]
fn test_expire_and_ttl() {
    let engine = KvEngine::new();
    let key = KvKey::new("ttl_test").unwrap();

    engine
        .set(&key, KvValue::String("value".to_string()), None)
        .unwrap();
    assert_eq!(engine.ttl(&key), -1); // No TTL

    // Set TTL
    assert_eq!(engine.expire(&key, 60), 1);
    let ttl = engine.ttl(&key);
    assert!(ttl > 55 && ttl <= 60);

    // Non-existent key
    let missing = KvKey::new("missing").unwrap();
    assert_eq!(engine.expire(&missing, 60), 0);
    assert_eq!(engine.ttl(&missing), -2);
}

#[test]
fn test_pexpire_and_pttl() {
    let engine = KvEngine::new();
    let key = KvKey::new("pttl_test").unwrap();

    engine
        .set(&key, KvValue::String("value".to_string()), None)
        .unwrap();
    assert_eq!(engine.pttl(&key), -1);

    // Set TTL in milliseconds
    assert_eq!(engine.pexpire(&key, 5000), 1);
    let pttl = engine.pttl(&key);
    assert!(pttl > 4500 && pttl <= 5000);
}

#[test]
fn test_persist() {
    let engine = KvEngine::new();
    let key = KvKey::new("persist_test").unwrap();

    // Set with TTL
    engine
        .set(
            &key,
            KvValue::String("value".to_string()),
            Some(Duration::from_secs(60)),
        )
        .unwrap();
    assert!(engine.ttl(&key) > 0);

    // Persist removes TTL
    assert_eq!(engine.persist(&key), 1);
    assert_eq!(engine.ttl(&key), -1);

    // Second persist returns 0 (no TTL to remove)
    assert_eq!(engine.persist(&key), 0);
}

#[test]
fn test_getex() {
    let engine = KvEngine::new();
    let key = KvKey::new("getex_test").unwrap();

    engine
        .set(&key, KvValue::String("hello".to_string()), None)
        .unwrap();

    // Get and set TTL
    let value = engine.getex(&key, Some(Duration::from_secs(30)), false);
    assert_eq!(value, Some(KvValue::String("hello".to_string())));
    let ttl = engine.ttl(&key);
    assert!(ttl > 25 && ttl <= 30);

    // Get and persist
    let value = engine.getex(&key, None, true);
    assert_eq!(value, Some(KvValue::String("hello".to_string())));
    assert_eq!(engine.ttl(&key), -1);
}

#[test]
fn test_expire_zero_deletes() {
    let engine = KvEngine::new();
    let key = KvKey::new("expire_zero").unwrap();

    engine
        .set(&key, KvValue::String("value".to_string()), None)
        .unwrap();
    assert!(engine.exists(&key));

    // EXPIRE 0 should delete
    engine.expire(&key, 0);
    assert!(!engine.exists(&key));
}

// ==================== Hash Operations Tests ====================

#[test]
fn test_hset_hget() {
    let engine = KvEngine::new();
    let key = KvKey::new("myhash").unwrap();

    // Set fields
    let added = engine
        .hset(
            &key,
            vec![
                ("field1".to_string(), KvValue::String("value1".to_string())),
                ("field2".to_string(), KvValue::Int(42)),
            ],
        )
        .unwrap();
    assert_eq!(added, 2);

    // Get single field
    assert_eq!(
        engine.hget(&key, "field1").unwrap(),
        Some(KvValue::String("value1".to_string()))
    );
    assert_eq!(engine.hget(&key, "field2").unwrap(), Some(KvValue::Int(42)));
    assert_eq!(engine.hget(&key, "missing").unwrap(), None);

    // Update existing field (should return 0 for added)
    let added = engine
        .hset(
            &key,
            vec![
                ("field1".to_string(), KvValue::String("updated".to_string())),
                ("field3".to_string(), KvValue::String("new".to_string())),
            ],
        )
        .unwrap();
    assert_eq!(added, 1); // Only field3 is new

    assert_eq!(
        engine.hget(&key, "field1").unwrap(),
        Some(KvValue::String("updated".to_string()))
    );
}

#[test]
fn test_hmget_hgetall() {
    let engine = KvEngine::new();
    let key = KvKey::new("myhash2").unwrap();

    engine
        .hset(
            &key,
            vec![
                ("a".to_string(), KvValue::String("1".to_string())),
                ("b".to_string(), KvValue::String("2".to_string())),
                ("c".to_string(), KvValue::String("3".to_string())),
            ],
        )
        .unwrap();

    // HMGET
    let values = engine.hmget(&key, &["a", "c", "missing"]).unwrap();
    assert_eq!(values.len(), 3);
    assert_eq!(values[0], Some(KvValue::String("1".to_string())));
    assert_eq!(values[1], Some(KvValue::String("3".to_string())));
    assert_eq!(values[2], None);

    // HGETALL
    let all = engine.hgetall(&key).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all.get("a"), Some(&KvValue::String("1".to_string())));
}

#[test]
fn test_hdel_hexists_hlen() {
    let engine = KvEngine::new();
    let key = KvKey::new("myhash3").unwrap();

    engine
        .hset(
            &key,
            vec![
                ("f1".to_string(), KvValue::Int(1)),
                ("f2".to_string(), KvValue::Int(2)),
                ("f3".to_string(), KvValue::Int(3)),
            ],
        )
        .unwrap();

    assert_eq!(engine.hlen(&key).unwrap(), 3);
    assert!(engine.hexists(&key, "f1").unwrap());
    assert!(!engine.hexists(&key, "missing").unwrap());

    // Delete fields
    let removed = engine.hdel(&key, &["f1", "f2", "missing"]).unwrap();
    assert_eq!(removed, 2);
    assert_eq!(engine.hlen(&key).unwrap(), 1);
    assert!(!engine.hexists(&key, "f1").unwrap());
}

#[test]
fn test_hash_type_mismatch() {
    let engine = KvEngine::new();
    let key = KvKey::new("string_key").unwrap();

    engine
        .set(&key, KvValue::String("not a hash".to_string()), None)
        .unwrap();

    let result = engine.hset(&key, vec![("f".to_string(), KvValue::Int(1))]);
    assert!(matches!(result, Err(KvError::TypeMismatch { .. })));
}

#[test]
fn test_hincrby() {
    let engine = KvEngine::new();
    let key = KvKey::new("counter_hash").unwrap();

    // HINCRBY on non-existent hash creates it
    let val = engine.hincrby(&key, "count", 5).unwrap();
    assert_eq!(val, 5);

    // HINCRBY increments existing field
    let val = engine.hincrby(&key, "count", 3).unwrap();
    assert_eq!(val, 8);

    // HINCRBY with negative decrement
    let val = engine.hincrby(&key, "count", -2).unwrap();
    assert_eq!(val, 6);

    // HINCRBY on new field
    let val = engine.hincrby(&key, "other", 10).unwrap();
    assert_eq!(val, 10);

    // Verify values stored correctly
    assert_eq!(engine.hget(&key, "count").unwrap(), Some(KvValue::Int(6)));
    assert_eq!(engine.hget(&key, "other").unwrap(), Some(KvValue::Int(10)));
}

#[test]
fn test_hincrby_type_error() {
    let engine = KvEngine::new();
    let key = KvKey::new("mixed_hash").unwrap();

    // Set a string field
    engine
        .hset(
            &key,
            vec![("name".to_string(), KvValue::String("test".to_string()))],
        )
        .unwrap();

    // HINCRBY on string field should fail
    let result = engine.hincrby(&key, "name", 1);
    assert!(matches!(result, Err(KvError::Storage(_))));
}

#[test]
fn test_hincrbyfloat() {
    let engine = KvEngine::new();
    let key = KvKey::new("float_hash").unwrap();

    // HINCRBYFLOAT on non-existent hash creates it
    let val = engine.hincrbyfloat(&key, "score", 1.5).unwrap();
    assert!((val - 1.5).abs() < f64::EPSILON);

    // HINCRBYFLOAT increments existing field
    let val = engine.hincrbyfloat(&key, "score", 2.5).unwrap();
    assert!((val - 4.0).abs() < f64::EPSILON);

    // HINCRBYFLOAT with negative
    let val = engine.hincrbyfloat(&key, "score", -1.0).unwrap();
    assert!((val - 3.0).abs() < f64::EPSILON);
}

#[test]
fn test_hincrbyfloat_from_int() {
    let engine = KvEngine::new();
    let key = KvKey::new("mixed_num_hash").unwrap();

    // Start with integer
    engine.hincrby(&key, "val", 10).unwrap();

    // HINCRBYFLOAT should convert int to float
    let val = engine.hincrbyfloat(&key, "val", 0.5).unwrap();
    assert!((val - 10.5).abs() < f64::EPSILON);

    // Value should now be Float
    let stored = engine.hget(&key, "val").unwrap().unwrap();
    assert!(matches!(stored, KvValue::Float(_)));
}

// ==================== List Operations Tests ====================

#[test]
fn test_lpush_rpush() {
    let engine = KvEngine::new();
    let key = KvKey::new("mylist").unwrap();

    // RPUSH creates list
    let len = engine
        .rpush(
            &key,
            vec![
                KvValue::String("a".to_string()),
                KvValue::String("b".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(len, 2);

    // LPUSH adds to head
    let len = engine
        .lpush(&key, vec![KvValue::String("c".to_string())])
        .unwrap();
    assert_eq!(len, 3);

    // List should be [c, a, b]
    let range = engine.lrange(&key, 0, -1).unwrap();
    assert_eq!(
        range,
        vec![
            KvValue::String("c".to_string()),
            KvValue::String("a".to_string()),
            KvValue::String("b".to_string()),
        ]
    );
}

#[test]
fn test_lpop_rpop() {
    let engine = KvEngine::new();
    let key = KvKey::new("poplist").unwrap();

    engine
        .rpush(
            &key,
            vec![
                KvValue::String("first".to_string()),
                KvValue::String("second".to_string()),
                KvValue::String("third".to_string()),
            ],
        )
        .unwrap();

    // LPOP from head
    assert_eq!(
        engine.lpop(&key),
        Some(KvValue::String("first".to_string()))
    );

    // RPOP from tail
    assert_eq!(
        engine.rpop(&key),
        Some(KvValue::String("third".to_string()))
    );

    // Only "second" remains
    assert_eq!(engine.llen(&key).unwrap(), 1);
    assert_eq!(
        engine.lpop(&key),
        Some(KvValue::String("second".to_string()))
    );

    // Empty now
    assert_eq!(engine.lpop(&key), None);
}

#[test]
fn test_lrange() {
    let engine = KvEngine::new();
    let key = KvKey::new("rangelist").unwrap();

    engine
        .rpush(
            &key,
            vec![
                KvValue::Int(0),
                KvValue::Int(1),
                KvValue::Int(2),
                KvValue::Int(3),
                KvValue::Int(4),
            ],
        )
        .unwrap();

    // Positive indices
    assert_eq!(
        engine.lrange(&key, 0, 2).unwrap(),
        vec![KvValue::Int(0), KvValue::Int(1), KvValue::Int(2)]
    );

    // Negative indices
    assert_eq!(
        engine.lrange(&key, -3, -1).unwrap(),
        vec![KvValue::Int(2), KvValue::Int(3), KvValue::Int(4)]
    );

    // Mixed
    assert_eq!(
        engine.lrange(&key, 1, -2).unwrap(),
        vec![KvValue::Int(1), KvValue::Int(2), KvValue::Int(3)]
    );

    // Out of bounds
    assert_eq!(
        engine.lrange(&key, 0, 100).unwrap(),
        vec![
            KvValue::Int(0),
            KvValue::Int(1),
            KvValue::Int(2),
            KvValue::Int(3),
            KvValue::Int(4)
        ]
    );
}

#[test]
fn test_list_type_mismatch() {
    let engine = KvEngine::new();
    let key = KvKey::new("not_a_list").unwrap();

    engine
        .set(&key, KvValue::String("string".to_string()), None)
        .unwrap();

    let result = engine.rpush(&key, vec![KvValue::Int(1)]);
    assert!(matches!(result, Err(KvError::TypeMismatch { .. })));
}

// ==================== Memory Eviction Tests ====================

#[test]
fn test_maxmemory_config() {
    let engine = KvEngine::new();

    assert_eq!(engine.get_maxmemory(), 0); // Default unlimited
    assert_eq!(engine.get_eviction_policy(), EvictionPolicy::AllKeysLru);

    engine.set_maxmemory(1024 * 1024);
    assert_eq!(engine.get_maxmemory(), 1024 * 1024);

    engine.set_eviction_policy(EvictionPolicy::NoEviction);
    assert_eq!(engine.get_eviction_policy(), EvictionPolicy::NoEviction);
}

#[test]
fn test_eviction_lru() {
    let engine = KvEngine::new();
    engine.set_eviction_policy(EvictionPolicy::AllKeysLru);

    // Set a small memory limit
    engine.set_maxmemory(500);

    // Add first key
    let key1 = KvKey::new("key1").unwrap();
    engine
        .set(&key1, KvValue::String("value1".to_string()), None)
        .unwrap();

    // Sleep to ensure different access times
    thread::sleep(Duration::from_millis(10));

    // Access key1 to make it more recently used
    engine.get(&key1);

    // Add more keys to trigger eviction
    for i in 2..10 {
        let key = KvKey::new(format!("key{}", i)).unwrap();
        let _ = engine.hset(
            &key,
            vec![("f".to_string(), KvValue::String("x".repeat(50)))],
        );
    }

    // Some keys should have been evicted
    let count = engine.len();
    assert!(count < 10, "Expected eviction to occur, got {} keys", count);
}

#[test]
fn test_eviction_noeviction_oom() {
    let engine = KvEngine::new();
    engine.set_eviction_policy(EvictionPolicy::NoEviction);

    // Fill up memory first
    for i in 0..5 {
        let key = KvKey::new(format!("fill{}", i)).unwrap();
        engine
            .set(&key, KvValue::String("x".repeat(100)), None)
            .unwrap();
    }

    // Now set a very small limit (less than current usage)
    let current = engine.estimate_memory();
    engine.set_maxmemory(current / 2);

    // Adding more should fail with OOM
    let key = KvKey::new("new_key").unwrap();
    let result = engine.hset(
        &key,
        vec![("f".to_string(), KvValue::String("x".repeat(200)))],
    );

    assert!(matches!(result, Err(KvError::OutOfMemory)));
}

#[test]
fn test_incremental_memory_accounting_accuracy() {
    let engine = KvEngine::new();
    assert_eq!(engine.estimate_memory(), 0);

    let k1 = KvKey::new("k1").unwrap();
    let k2 = KvKey::new("k2").unwrap();

    engine
        .set(&k1, KvValue::String("hello".repeat(100)), None)
        .unwrap();
    let mem1 = engine.estimate_memory();
    assert!(mem1 > 500);

    // Overwrite with smaller value -> memory must decrease
    engine.set(&k1, KvValue::Int(1), None).unwrap();
    let mem2 = engine.estimate_memory();
    assert!(mem2 < mem1);

    // Add collection and then delete -> memory must return to 0
    engine
        .sadd(&k2, vec!["m1".to_string(), "m2".to_string()])
        .unwrap();
    assert!(engine.estimate_memory() > mem2);

    engine.delete(&k1);
    engine
        .srem(&k2, vec!["m1".to_string(), "m2".to_string()])
        .unwrap();
    assert_eq!(engine.estimate_memory(), 0);
}

#[test]
fn test_tiered_blob_spill_on_write_and_memory_pressure() {
    use crate::persistence::blob_store::BlobStore;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let engine = KvEngine::with_shards(16);
    let store = Arc::new(BlobStore::new(temp_dir.path(), 1024, 0).unwrap());
    engine.enable_blob_store(store);

    // 1. Write a 4 KiB payload (>= 1024 spill threshold)
    let big_key = KvKey::new("big_blob").unwrap();
    let payload = vec![7u8; 4096];
    engine
        .set(&big_key, KvValue::Bytes(payload.clone()), None)
        .unwrap();

    assert_eq!(engine.used_disk_bytes(), 4096);
    assert_eq!(engine.spilled_keys(), 1);
    // RAM footprint of the spilled entry should be tiny (< 256 bytes)
    assert!(engine.estimate_memory() < 256);
    assert_eq!(engine.get(&big_key), Some(KvValue::Bytes(payload)));

    // Deleting the key must unlink the blob and drop disk bytes to 0
    assert!(engine.delete(&big_key));
    assert_eq!(engine.used_disk_bytes(), 0);
    assert_eq!(engine.estimate_memory(), 0);

    // 2. Write smaller keys (< 1024) in RAM, then trigger memory-pressure spill-before-evict
    let k1 = KvKey::new("ram_blob_1").unwrap();
    let k2 = KvKey::new("ram_blob_2").unwrap();
    engine
        .set(&k1, KvValue::Bytes(vec![1u8; 512]), None)
        .unwrap();
    assert_eq!(engine.used_disk_bytes(), 0);

    // Set maxmemory below current RAM so next write triggers spill-before-evict on k1
    let ram_used = engine.estimate_memory();
    engine.set_maxmemory(ram_used);

    engine
        .set(&k2, KvValue::Bytes(vec![2u8; 256]), None)
        .unwrap();

    // k1 was spilled to disk rather than deleted, so BOTH k1 and k2 are still readable!
    assert_eq!(engine.used_disk_bytes(), 512);
    assert_eq!(engine.get(&k1), Some(KvValue::Bytes(vec![1u8; 512])));
    assert_eq!(engine.get(&k2), Some(KvValue::Bytes(vec![2u8; 256])));
}
