use super::{Entry, Shard};
use crate::error::KvError;
use crate::types::KvValue;

impl Shard {
    // ==================== List Operations ====================

    /// Push elements to head of list (LPUSH)
    pub fn lpush(&self, key: &str, values: Vec<KvValue>) -> Result<usize, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::List(list) => {
                        for value in values {
                            list.insert(0, value);
                        }
                        entry.version += 1;
                        let len = list.len();
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(len)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "List".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                let mut list = Vec::new();
                for value in values {
                    list.insert(0, value);
                }
                let len = list.len();
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::List(list), None),
                );
                Ok(len)
            }
        }
    }

    /// Push elements to tail of list (RPUSH)
    pub fn rpush(&self, key: &str, values: Vec<KvValue>) -> Result<usize, KvError> {
        let mut guard = self.data.write();

        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::List(list) => {
                        list.extend(values);
                        entry.version += 1;
                        let len = list.len();
                        self.refresh_entry_ram(key.len(), entry);
                        Ok(len)
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "List".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => {
                let len = values.len();
                self.insert_entry(
                    &mut guard,
                    key.to_string(),
                    Entry::new(KvValue::List(values), None),
                );
                Ok(len)
            }
        }
    }

    /// Pop element from head of list (LPOP)
    pub fn lpop(&self, key: &str) -> Option<KvValue> {
        let mut guard = self.data.write();
        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::List(list) if !list.is_empty() => {
                        entry.version += 1;
                        let value = list.remove(0);
                        if list.is_empty() {
                            self.remove_entry(&mut guard, key);
                        } else {
                            self.refresh_entry_ram(key.len(), entry);
                        }
                        Some(value)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Pop element from tail of list (RPOP)
    pub fn rpop(&self, key: &str) -> Option<KvValue> {
        let mut guard = self.data.write();
        match guard.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &mut entry.value {
                    KvValue::List(list) if !list.is_empty() => {
                        entry.version += 1;
                        let value = list.pop();
                        if list.is_empty() {
                            self.remove_entry(&mut guard, key);
                        } else {
                            self.refresh_entry_ram(key.len(), entry);
                        }
                        value
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Get range of elements from list (LRANGE)
    pub fn lrange(&self, key: &str, start: i64, stop: i64) -> Result<Vec<KvValue>, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch();
                match &entry.value {
                    KvValue::List(list) => {
                        let len = list.len() as i64;
                        if len == 0 {
                            return Ok(Vec::new());
                        }

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
                            Ok(Vec::new())
                        } else {
                            Ok(list[start_idx..stop_idx].to_vec())
                        }
                    }
                    other => Err(KvError::TypeMismatch {
                        expected: "List".to_string(),
                        actual: format!("{:?}", std::mem::discriminant(other)),
                    }),
                }
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Get length of list (LLEN)
    pub fn llen(&self, key: &str) -> Result<usize, KvError> {
        let guard = self.data.read();
        match guard.get(key) {
            Some(entry) if !entry.is_expired() => match &entry.value {
                KvValue::List(list) => Ok(list.len()),
                other => Err(KvError::TypeMismatch {
                    expected: "List".to_string(),
                    actual: format!("{:?}", std::mem::discriminant(other)),
                }),
            },
            _ => Ok(0),
        }
    }
}
