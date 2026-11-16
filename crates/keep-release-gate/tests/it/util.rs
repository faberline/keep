//! Tests for the file and digest helpers.

use keep_release_gate::util::{
    is_ascii_digits, is_int, is_lower_hex, is_semver, load_json, read_regular, require_digest,
    sha256_hex,
};
use serde_json::json;

#[test]
fn read_regular_refuses_a_symlink_a_directory_and_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.json");
    std::fs::write(&real, b"{}").unwrap();
    assert_eq!(read_regular(&real, "label").unwrap(), b"{}");

    let missing = dir.path().join("missing.json");
    assert_eq!(
        read_regular(&missing, "candidate manifest")
            .unwrap_err()
            .to_string(),
        format!(
            "GKE receipt refused: candidate manifest must be a regular file: {}",
            missing.display()
        )
    );
    assert!(read_regular(dir.path(), "label").is_err());

    #[cfg(unix)]
    {
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            read_regular(&link, "label").unwrap_err().to_string(),
            format!(
                "GKE receipt refused: label must be a regular file: {}",
                link.display()
            )
        );
    }
}

#[test]
fn load_json_refuses_invalid_and_non_object() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.json");

    std::fs::write(&path, b"{not json").unwrap();
    let err = load_json(&path, "candidate manifest")
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("GKE receipt refused: candidate manifest is not valid JSON: "),
        "{err}"
    );

    std::fs::write(&path, b"[1]").unwrap();
    assert_eq!(
        load_json(&path, "candidate manifest")
            .unwrap_err()
            .to_string(),
        "GKE receipt refused: candidate manifest must be a JSON object"
    );

    std::fs::write(&path, b"{\"a\":1}").unwrap();
    let (raw, map) = load_json(&path, "candidate manifest").unwrap();
    assert_eq!(raw, b"{\"a\":1}");
    assert_eq!(map["a"], json!(1));
}

#[test]
fn require_digest_refuses_63_hex_digits() {
    let good = format!("sha256:{}", "a".repeat(64));
    assert_eq!(
        require_digest(Some(&json!(good)), "image digest").unwrap(),
        good
    );

    let short = json!(format!("sha256:{}", "a".repeat(63)));
    assert_eq!(
        require_digest(Some(&short), "image digest")
            .unwrap_err()
            .to_string(),
        "GKE receipt refused: image digest is not an immutable SHA-256 digest"
    );
    assert!(require_digest(Some(&json!(5)), "image digest").is_err());
    assert!(require_digest(None, "image digest").is_err());
}

#[test]
fn helpers_match_the_python_patterns() {
    assert!(is_semver("1.2.3"));
    assert!(!is_semver("1.2"));
    assert!(!is_semver("1..3"));
    assert!(!is_semver("a.b.c"));

    assert!(is_lower_hex("ab", 2));
    assert!(!is_lower_hex("AB", 2));
    assert!(!is_lower_hex("abc", 2));

    assert!(!is_ascii_digits(""));
    assert!(is_ascii_digits("123"));
    assert!(!is_ascii_digits("12a"));

    assert!(is_int(&json!(3)));
    assert!(!is_int(&json!(true)));
    assert!(!is_int(&json!(1.5)));

    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
