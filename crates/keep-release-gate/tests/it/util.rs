use keep_release_gate::util::{
    is_ascii_digits, is_lower_hex, is_semver, load_json, read_regular, require_digest, sha256_hex,
};
use serde_json::Value;

#[test]
fn read_regular_refuses_a_symlink_and_a_missing_file() {
    let dir = tempfile::tempdir().expect("tempdir");

    let missing = dir.path().join("missing.json");
    let message = read_regular(&missing, "candidate manifest")
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        format!(
            "GKE receipt refused: candidate manifest must be a regular file: {}",
            missing.display()
        )
    );

    let real = dir.path().join("real.json");
    std::fs::write(&real, b"{}").expect("write real file");
    assert_eq!(
        read_regular(&real, "candidate manifest").expect("regular file"),
        b"{}".to_vec()
    );

    #[cfg(unix)]
    {
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let message = read_regular(&link, "candidate manifest")
            .unwrap_err()
            .to_string();
        assert_eq!(
            message,
            format!(
                "GKE receipt refused: candidate manifest must be a regular file: {}",
                link.display()
            )
        );
    }
}

#[test]
fn load_json_refuses_invalid_and_non_object() {
    let dir = tempfile::tempdir().expect("tempdir");

    let invalid = dir.path().join("invalid.json");
    std::fs::write(&invalid, b"{not json").expect("write invalid");
    let message = load_json(&invalid, "candidate manifest")
        .unwrap_err()
        .to_string();
    assert!(
        message.starts_with("GKE receipt refused: candidate manifest is not valid JSON: "),
        "unexpected message: {message}"
    );

    let array = dir.path().join("array.json");
    std::fs::write(&array, b"[1]").expect("write array");
    let message = load_json(&array, "candidate manifest")
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "GKE receipt refused: candidate manifest must be a JSON object"
    );

    let object = dir.path().join("object.json");
    std::fs::write(&object, br#"{"a":1}"#).expect("write object");
    let (raw, map) = load_json(&object, "candidate manifest").expect("object");
    assert_eq!(raw, br#"{"a":1}"#.to_vec());
    assert_eq!(map.get("a"), Some(&Value::from(1)));
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

    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn require_digest_refuses_63_hex_digits() {
    let short = Value::from(format!("sha256:{}", "a".repeat(63)));
    let message = require_digest(Some(&short), "candidate root digest")
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "GKE receipt refused: candidate root digest is not an immutable SHA-256 digest"
    );

    let good = Value::from(format!("sha256:{}", "a".repeat(64)));
    assert_eq!(
        require_digest(Some(&good), "candidate root digest").expect("digest"),
        format!("sha256:{}", "a".repeat(64))
    );
}
