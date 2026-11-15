//! File, JSON and digest helpers shared by the release checks.

use crate::refusal::{refuse, Result};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Lowercase hex SHA-256 of `raw`.
pub fn sha256_hex(raw: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw);
    format!("{:x}", hasher.finalize())
}

/// Read a regular file. A symlink, a directory or a missing path is refused.
pub fn read_regular(path: &Path, label: &str) -> Result<Vec<u8>> {
    let refused = || {
        refuse(format!(
            "{label} must be a regular file: {}",
            path.display()
        ))
    };
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return refused();
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return refused();
    }
    match std::fs::read(path) {
        Ok(raw) => Ok(raw),
        Err(_) => refused(),
    }
}

/// Read and parse a JSON object. Returns the raw bytes and the object.
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

/// The value must be `sha256:` followed by exactly 64 lowercase hex digits.
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

/// Exactly `len` characters, each one of `0-9a-f`.
pub fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Non-empty, and every character is an ASCII digit.
pub fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// Three non-empty digit groups separated by two dots, like `1.2.3`.
pub fn is_semver(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| is_ascii_digits(part))
}
