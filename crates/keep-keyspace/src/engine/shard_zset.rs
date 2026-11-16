use super::{Entry, Shard};
use crate::error::KvError;
use crate::types::KvValue;

impl Shard {
    // ==================== Sorted Set Operations ====================

    /// Add members with scores to sorted set (ZADD)
    pub fn zadd(&self, key: &str, members: Vec<(String, f64)>) -> Result<usize, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::SortedSet(ss) => {
                        let mut added = 0;
                        for (member, score) in members {
                            if !ss.contains_key(&member) {
                                added += 1;
                            }
                            ss.insert(member, score);
                        }
                        entry.version += 1;
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(added)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "SortedSet".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                let mut ss = std::collections::BTreeMap::new();
                let mut added = 0;
                for (member, score) in members {
                    if !ss.contains_key(&member) {
                        added += 1;
                    }
                    ss.insert(member, score);
                }
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::SortedSet(ss), None),
                );
                Ok(added)
            }
        }
    }

    /// Remove members from sorted set (ZREM)
    pub fn zrem(&self, key: &str, members: Vec<String>) -> Result<usize, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::SortedSet(ss) => {
                        let mut removed = 0;
                        for member in members {
                            if ss.remove(&member).is_some() {
                                removed += 1;
                            }
                        }
                        if ss.is_empty() {
                            self.remove_entry(&mut guard, key);
                        } else {
                            self.refresh_entry_ram(key.len(), entry);
                        }
                        Ok(removed)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "SortedSet".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(0),
        }
    }

    /// Get score of member (ZSCORE)
    pub fn zscore(&self, key: &str, member: &str) -> Result<Option<f64>, KvError> {
        let guard = self.data.read();

        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::SortedSet(ss) => Ok(ss.get(member).copied()),
                    other => Err(KvError::TypeMismatch {
                        expected: "SortedSet".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(None),
        }
    }

    /// Increment score of member (ZINCRBY)
    pub fn zincrby(&self, key: &str, member: &str, increment: f64) -> Result<f64, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::SortedSet(ss) => {
                        let new_score = ss.get(member).unwrap_or(&0.0) + increment;
                        ss.insert(member.to_string(), new_score);
                        entry.version += 1;
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(new_score)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "SortedSet".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                let mut ss = std::collections::BTreeMap::new();
                ss.insert(member.to_string(), increment);
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::SortedSet(ss), None),
                );
                Ok(increment)
            }
        }
    }

    /// Get rank of member (0-based, ascending by score) (ZRANK)
    pub fn zrank(&self, key: &str, member: &str) -> Result<Option<usize>, KvError> {
        let guard = self.data.read();

        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::SortedSet(ss) => {
                        let mut items: Vec<_> = ss.iter().collect();
                        items.sort_by(|a, b| {
                            a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        Ok(items.iter().position(|(m, _)| *m == member))
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "SortedSet".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(None),
        }
    }

    /// Get range by rank (ZRANGE)
    pub fn zrange(&self, key: &str, start: i64, stop: i64) -> Result<Vec<(String, f64)>, KvError> {
        let guard = self.data.read();

        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::SortedSet(ss) => {
                        let mut items: Vec<_> = ss.iter().map(|(m, s)| (m.clone(), *s)).collect();
                        items.sort_by(|a, b| {
                            a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                        });

                        let len = items.len() as i64;
                        let start_idx = if start < 0 {
                            (len + start).max(0)
                        } else {
                            start.min(len)
                        } as usize;
                        let stop_idx = if stop < 0 {
                            (len + stop + 1).max(0)
                        } else {
                            (stop + 1).min(len)
                        } as usize;

                        if start_idx >= stop_idx {
                            return Ok(Vec::new());
                        }

                        Ok(items[start_idx..stop_idx].to_vec())
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "SortedSet".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Get cardinality of sorted set (ZCARD)
    pub fn zcard(&self, key: &str) -> Result<usize, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => match &entry.value {
                KvValue::SortedSet(ss) => Ok(ss.len()),
                other => Err(KvError::TypeMismatch {
                    expected: "SortedSet".to_string(),
                    actual: format!("{:?}", std::mem::discriminant(other)),
                }),
            },
            _ => Ok(0),
        }
    }
}
