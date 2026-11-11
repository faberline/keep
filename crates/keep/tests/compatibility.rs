//! Version compatibility policy checks (issue #71).
//!
//! `docs/product/compatibility.md` states the on-disk format versions. These
//! tests keep that page true, and check that keep refuses a snapshot or WAL file
//! it does not know, without changing the file.

use keep::persistence::recovery::RecoveryManager;
use keep::persistence::snapshot::SNAPSHOT_VERSION;
use keep::persistence::PersistenceError;
use tempfile::TempDir;

const DOC: &str = include_str!("../../../docs/product/compatibility.md");

#[test]
fn doc_states_the_real_format_versions() {
    assert!(
        DOC.contains(&format!("snapshot file version: {SNAPSHOT_VERSION}")),
        "snapshot file version in the doc must match SNAPSHOT_VERSION"
    );
    assert!(
        DOC.contains(&format!(
            "backup payload version: {}",
            keep::backup::KEEP_SNAPSHOT_VERSION
        )),
        "backup payload version in the doc must match KEEP_SNAPSHOT_VERSION"
    );
    assert!(
        DOC.contains("KVSN0001"),
        "snapshot magic must be in the doc"
    );
    assert!(
        DOC.contains("KVWAL001"),
        "retired WAL magic must be in the doc"
    );
}

#[test]
fn doc_has_every_required_section() {
    for s in [
        "## On-disk format",
        "## Wire compatibility",
        "## Upgrade order",
        "## Announcing a format change",
        "/openapi.json",
        "N+1",
    ] {
        assert!(DOC.contains(s), "compatibility doc is missing `{s}`");
    }
}

#[test]
fn recovery_refuses_a_snapshot_with_an_unknown_version() {
    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path();

    // Magic, then version 999, zero padded to the 72-byte header size.
    let mut bytes = b"KVSN0001".to_vec();
    bytes.extend_from_slice(&999u32.to_be_bytes());
    bytes.resize(72, 0);
    let snap = data_dir.join("snapshot-1000.snap");
    std::fs::write(&snap, &bytes).unwrap();

    let result = RecoveryManager::recover(data_dir, 16);
    assert!(matches!(
        result,
        Err(PersistenceError::UnsupportedVersion(999))
    ));
    assert_eq!(std::fs::read(&snap).unwrap(), bytes, "file must not change");
}

#[test]
fn recovery_refuses_a_snapshot_with_an_unknown_magic() {
    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path();

    let mut bytes = b"KVSN9999".to_vec();
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.resize(72, 0);
    let snap = data_dir.join("snapshot-1000.snap");
    std::fs::write(&snap, &bytes).unwrap();

    let result = RecoveryManager::recover(data_dir, 16);
    assert!(matches!(
        result,
        Err(PersistenceError::CorruptedSnapshot(_) | PersistenceError::InvalidMagic { .. })
    ));
    assert_eq!(std::fs::read(&snap).unwrap(), bytes, "file must not change");
}

#[test]
fn recovery_refuses_a_retired_wal_and_leaves_it_alone() {
    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path();

    let mut legacy = b"KVWAL001".to_vec();
    legacy.extend_from_slice(&[0u8; 24]);
    let wal_path = data_dir.join("wal-current.log");
    std::fs::write(&wal_path, &legacy).unwrap();

    let result = RecoveryManager::recover(data_dir, 16);
    assert!(matches!(
        result,
        Err(PersistenceError::LegacyWalFormat { .. })
    ));
    assert_eq!(std::fs::read(&wal_path).unwrap(), legacy, "not truncated");
}
