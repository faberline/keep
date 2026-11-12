//! Version compatibility policy (`docs/product/compatibility.md`): the doc names
//! the real format versions and the required sections, and recovery refuses a
//! file in a version or format it does not know, without changing the file.

use keep::persistence::recovery::RecoveryManager;
use keep::persistence::PersistenceError;
use tempfile::TempDir;

const DOC: &str = include_str!("../../../docs/product/compatibility.md");

#[test]
fn doc_states_the_real_format_versions() {
    assert!(DOC.contains(&format!(
        "snapshot file version: {}",
        keep::persistence::snapshot::SNAPSHOT_VERSION
    )));
    assert!(DOC.contains(&format!(
        "backup payload version: {}",
        keep::backup::KEEP_SNAPSHOT_VERSION
    )));
    assert!(DOC.contains("KVSN0001"));
    assert!(DOC.contains("KVWAL001"));
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
        assert!(DOC.contains(s), "{s}");
    }
}

#[test]
fn recovery_refuses_a_snapshot_with_an_unknown_version() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("snapshot-1000.snap");
    let mut b = b"KVSN0001".to_vec();
    b.extend_from_slice(&999u32.to_be_bytes());
    b.resize(64, 0);
    std::fs::write(&path, &b).unwrap();

    let result = RecoveryManager::recover(dir.path(), 16);
    assert!(matches!(
        result,
        Err(PersistenceError::UnsupportedVersion(999))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b);
}

#[test]
fn recovery_refuses_a_snapshot_with_an_unknown_magic() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("snapshot-1000.snap");
    let mut b = b"KVSN9999".to_vec();
    b.extend_from_slice(&2u32.to_be_bytes());
    b.resize(64, 0);
    std::fs::write(&path, &b).unwrap();

    let result = RecoveryManager::recover(dir.path(), 16);
    assert!(matches!(
        result,
        Err(PersistenceError::CorruptedSnapshot(_) | PersistenceError::InvalidMagic { .. })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b);
}

#[test]
fn recovery_refuses_a_retired_wal_and_leaves_it_alone() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal-current.log");
    let mut b = b"KVWAL001".to_vec();
    b.extend_from_slice(&[0u8; 24]);
    std::fs::write(&path, &b).unwrap();

    let result = RecoveryManager::recover(dir.path(), 16);
    assert!(matches!(
        result,
        Err(PersistenceError::LegacyWalFormat { .. })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b);
}
