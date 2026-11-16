use super::{Entry, Shard};
use crate::error::KvError;
use crate::types::KvValue;

impl Shard {
    // ==================== Set Operations ====================

    /// Add members to a set (SADD)
    pub fn sadd(&self, key: &str, members: Vec<String>) -> Result<usize, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::Set(set) => {
                        let mut added = 0;
                        for member in members {
                            if set.insert(member) {
                                added += 1;
                            }
                        }
                        entry.version += 1;
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(added)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "Set".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                let mut set = std::collections::HashSet::new();
                let mut added = 0;
                for member in members {
                    if set.insert(member) {
                        added += 1;
                    }
                }
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::Set(set), None),
                );
                Ok(added)
            }
        }
    }

    /// Remove members from a set (SREM)
    pub fn srem(&self, key: &str, members: Vec<String>) -> Result<usize, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::Set(set) => {
                        let mut removed = 0;
                        for member in members {
                            if set.remove(&member) {
                                removed += 1;
                            }
                        }
                        if set.is_empty() {
                            self.remove_entry(&mut guard, key);
                        } else {
                            self.refresh_entry_ram(key.len(), entry);
                        }
                        Ok(removed)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "Set".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(0),
        }
    }

    /// Get all members of a set (SMEMBERS)
    pub fn smembers(&self, key: &str) -> Result<Vec<String>, KvError> {
        let guard = self.data.read();

        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::Set(set) => Ok(set.iter().cloned().collect()),
                    other => Err(KvError::TypeMismatch {
                        expected: "Set".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Check if member exists in set (SISMEMBER)
    pub fn sismember(&self, key: &str, member: &str) -> Result<bool, KvError> {
        let guard = self.data.read();

        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::Set(set) => Ok(set.contains(member)),
                    other => Err(KvError::TypeMismatch {
                        expected: "Set".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(false),
        }
    }

    /// Get cardinality (size) of set (SCARD)
    pub fn scard(&self, key: &str) -> Result<usize, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => match &entry.value {
                KvValue::Set(set) => Ok(set.len()),
                other => Err(KvError::TypeMismatch {
                    expected: "Set".to_string(),
                    actual: format!("{:?}", std::mem::discriminant(other)),
                }),
            },
            _ => Ok(0),
        }
    }
}
