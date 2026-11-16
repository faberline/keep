//! File and digest helpers, ported from the receipt script.

use crate::refusal::{refuse, Refusal, Result};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Lowercase hex SHA-256 of `raw`.
pub fn sha256_hex(raw: &[u8]) -> String {
    format!("{:x}", Sha256::digest(raw))
}

/// The bytes of a regular file. A symlink, a directory or a missing path is refused.
pub fn read_regular(path: &Path, label: &str) -> Result<Vec<u8>> {
    let message = || format!("{label} must be a regular file: {}", path.display());
    let is_regular = std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false);
    if !is_regular {
        return refuse(message());
    }
    std::fs::read(path).map_err(|_| Refusal(message()))
}

/// The raw bytes and the parsed object of a JSON file.
pub fn load_json(path: &Path, label: &str) -> Result<(Vec<u8>, Map<String, Value>)> {
    let raw = read_regular(path, label)?;
    let value: Value = match serde_json::from_slice(&raw) {
        Ok(value) => value,
        Err(error) => return refuse(format!("{label} is not valid JSON: {error}")),
    };
    match value {
        Value::Object(map) => Ok((raw, map)),
        _ => refuse(format!("{label} must be a JSON object")),
    }
}

/// The value as `sha256:` plus exactly 64 lowercase hex digits, or a refusal.
pub fn require_digest(value: Option<&Value>, label: &str) -> Result<String> {
    match value.and_then(Value::as_str) {
        Some(digest)
            if digest
                .strip_prefix("sha256:")
                .is_some_and(|hex| is_lower_hex(hex, 64)) =>
        {
            Ok(digest.to_string())
        }
        _ => refuse(format!("{label} is not an immutable SHA-256 digest")),
    }
}

/// True when `value` has exactly `len` characters, all lowercase hex.
pub fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// True when `value` is non-empty and every character is an ASCII digit.
pub fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// True when `value` is three ASCII-digit parts separated by two dots.
pub fn is_semver(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| is_ascii_digits(part))
}

/// True for a JSON integer, not a bool and not a fraction.
pub fn is_int(value: &Value) -> bool {
    matches!(value, Value::Number(n) if n.is_i64() || n.is_u64())
}
