//! KV store data types
//!
//! Supports high-precision numerics and Redis-compatible collections.

use std::collections::{BTreeMap, HashSet};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Key type for KV store (max 256 UTF-8 characters)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KvKey(String);

impl KvKey {
    /// Create a new key, validating length constraints
    pub fn new(key: impl Into<String>) -> Result<Self, crate::error::KvError> {
        let key = key.into();
        if key.len() > 256 {
            return Err(crate::error::KvError::KeyTooLong(key.len()));
        }
        if key.is_empty() {
            return Err(crate::error::KvError::EmptyKey);
        }
        Ok(Self(key))
    }

    /// Get the key as a string slice
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Value types supported by the KV store
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum KvValue {
    /// 64-bit signed integer
    Int(i64),
    /// 64-bit floating point
    Float(f64),
    /// 128-bit fixed-point decimal for financial precision
    Decimal(Decimal),
    /// UTF-8 string
    String(String),
    /// Binary data
    Bytes(Vec<u8>),
    /// Ordered list of values
    List(Vec<KvValue>),
    /// Key-value map (Hash in Redis)
    Map(std::collections::HashMap<String, KvValue>),
    /// Unordered set of unique strings
    Set(HashSet<String>),
    /// Sorted set with scores (member -> score)
    SortedSet(BTreeMap<String, f64>),
    /// Null/None value
    Null,
}

/// Map native JSON into the engine value type.
///
/// - numbers: integral -> `Int`, fractional -> `Float`
/// - bool -> `Int` (`true`=1, `false`=0)
/// - string -> `String`; array -> `List`; object -> `Map`; null -> `Null`
///
/// Opaque byte blobs do not arrive here — they use the octet-stream path.
pub fn json_to_kv(v: serde_json::Value) -> KvValue {
    match v {
        serde_json::Value::Null => KvValue::Null,
        serde_json::Value::Bool(b) => KvValue::Int(if b { 1 } else { 0 }),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                KvValue::Int(i)
            } else {
                KvValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => KvValue::String(s),
        serde_json::Value::Array(a) => KvValue::List(a.into_iter().map(json_to_kv).collect()),
        serde_json::Value::Object(o) => {
            KvValue::Map(o.into_iter().map(|(k, v)| (k, json_to_kv(v))).collect())
        }
    }
}

/// Map an engine value back into native JSON.
///
/// `Decimal` is rendered as a string to preserve precision. `Bytes` is rendered
/// as an array of `u8` (the efficient transport for blobs is the octet-stream
/// path, not JSON).
pub fn kv_to_json(v: KvValue) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        KvValue::Null => J::Null,
        KvValue::Int(i) => J::from(i),
        KvValue::Float(f) => serde_json::Number::from_f64(f)
            .map(J::Number)
            .unwrap_or(J::Null),
        KvValue::Decimal(d) => J::String(d.to_string()),
        KvValue::String(s) => J::String(s),
        KvValue::Bytes(b) => J::Array(b.into_iter().map(J::from).collect()),
        KvValue::List(l) => J::Array(l.into_iter().map(kv_to_json).collect()),
        KvValue::Map(m) => J::Object(m.into_iter().map(|(k, v)| (k, kv_to_json(v))).collect()),
        KvValue::Set(s) => J::Array(s.into_iter().map(J::String).collect()),
        KvValue::SortedSet(z) => J::Object(
            z.into_iter()
                .map(|(k, score)| {
                    (
                        k,
                        serde_json::Number::from_f64(score)
                            .map(J::Number)
                            .unwrap_or(J::Null),
                    )
                })
                .collect(),
        ),
    }
}
