use super::{Entry, Shard};
use crate::error::KvError;
use crate::types::KvValue;

impl Shard {
    // ==================== Hash Operations ====================

    /// Set fields in a hash (HSET/HMSET)
    /// Returns number of new fields added (not updated)
    pub fn hset(&self, key: &str, fields: Vec<(String, KvValue)>) -> Result<usize, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::Map(map) => {
                        let mut added = 0;
                        for (field, value) in fields {
                            if !map.contains_key(&field) {
                                added += 1;
                            }
                            map.insert(field, value);
                        }
                        entry.version += 1;
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(added)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "Map".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                let mut map = std::collections::HashMap::new();
                let count = fields.len();
                for (field, value) in fields {
                    map.insert(field, value);
                }
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::Map(map), None),
                );
                Ok(count)
            }
        }
    }

    /// Get a field from a hash (HGET)
    pub fn hget(&self, key: &str, field: &str) -> Result<Option<KvValue>, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::Map(map) => Ok(map.get(field).cloned()),
                    other => Err(KvError::TypeMismatch {
                        expected: "Map".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(None),
        }
    }

    /// Get multiple fields from a hash (HMGET)
    pub fn hmget(&self, key: &str, fields: &[&str]) -> Result<Vec<Option<KvValue>>, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::Map(map) => Ok(fields.iter().map(|f| map.get(*f).cloned()).collect()),
                    other => Err(KvError::TypeMismatch {
                        expected: "Map".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(fields.iter().map(|_| None).collect()),
        }
    }

    /// Get all fields and values from a hash (HGETALL)
    pub fn hgetall(
        &self,
        key: &str,
    ) -> Result<std::collections::HashMap<String, KvValue>, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::Map(map) => Ok(map.clone()),
                    other => Err(KvError::TypeMismatch {
                        expected: "Map".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(std::collections::HashMap::new()),
        }
    }

    /// Delete fields from a hash (HDEL)
    pub fn hdel(&self, key: &str, fields: &[&str]) -> Result<usize, KvError> {
        let mut guard = self.data.write();
        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::Map(map) => {
                        let mut removed = 0;
                        for field in fields {
                            if map.remove(*field).is_some() {
                                removed += 1;
                            }
                        }
                        entry.version += 1;
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(removed)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "Map".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(0),
        }
    }

    /// Check if a field exists in a hash (HEXISTS)
    pub fn hexists(&self, key: &str, field: &str) -> Result<bool, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => match &entry.value {
                KvValue::Map(map) => Ok(map.contains_key(field)),
                other => Err(KvError::TypeMismatch {
                    expected: "Map".to_string(),
                    actual: format!("{:?}", std::mem::discriminant(other)),
                }),
            },
            _ => Ok(false),
        }
    }

    /// Get number of fields in a hash (HLEN)
    pub fn hlen(&self, key: &str) -> Result<usize, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => match &entry.value {
                KvValue::Map(map) => Ok(map.len()),
                other => Err(KvError::TypeMismatch {
                    expected: "Map".to_string(),
                    actual: format!("{:?}", std::mem::discriminant(other)),
                }),
            },
            _ => Ok(0),
        }
    }

    /// Increment integer value of a hash field (HINCRBY)
    pub fn hincrby(&self, key: &str, field: &str, increment: i64) -> Result<i64, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::Map(map) => {
                        let new_val = match map.get(field) {
                            Some(KvValue::Int(n)) => n.checked_add(increment).ok_or_else(|| {
                                KvError::Storage("increment would overflow".to_string())
                            })?,
                            Some(_) => {
                                return Err(KvError::Storage(
                                    "hash value is not an integer".to_string(),
                                ));
                            }
                            None => increment,
                        };
                        map.insert(field.to_string(), KvValue::Int(new_val));
                        entry.version += 1;
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(new_val)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "Map".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                let mut map = std::collections::HashMap::new();
                map.insert(field.to_string(), KvValue::Int(increment));
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::Map(map), None),
                );
                Ok(increment)
            }
        }
    }

    /// Increment float value of a hash field (HINCRBYFLOAT)
    pub fn hincrbyfloat(&self, key: &str, field: &str, increment: f64) -> Result<f64, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::Map(map) => {
                        let current = match map.get(field) {
                            Some(KvValue::Float(f)) => *f,
                            Some(KvValue::Int(n)) => *n as f64,
                            Some(_) => {
                                return Err(KvError::Storage(
                                    "hash value is not a number".to_string(),
                                ));
                            }
                            None => 0.0,
                        };
                        let new_val = current + increment;
                        if new_val.is_nan() || new_val.is_infinite() {
                            return Err(KvError::Storage(
                                "increment would produce infinity or NaN".to_string(),
                            ));
                        }
                        map.insert(field.to_string(), KvValue::Float(new_val));
                        entry.version += 1;
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(new_val)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "Map".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                let mut map = std::collections::HashMap::new();
                map.insert(field.to_string(), KvValue::Float(increment));
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::Map(map), None),
                );
                Ok(increment)
            }
        }
    }
}
