// HANDWRITE-BEGIN gap="missing-generator:unit-test:42f5646b" tracker="pending-tracker" reason="R1/R2: snapshot_bytes round-trips the engine's key/value set, and run_backup_once writes then prunes an artifact at a file:// destination."
//! keep backup adoption tests (#776).
//!
//! - R1: `snapshot_bytes` serializes the engine's live key/value set into a
//!   versioned `KeepSnapshot` that round-trips.
//! - R2: a `file://` destination resolves to a `LocalFsSink`; `run_backup_once`
//!   writes the payload as an artifact and `RetentionPolicy` pruning removes
//!   aged objects. `run_backup` exercises the full recover → serialize → sink
//!   path the `keep backup` verb / CronJob runner uses.
//!
//! @spec .aw/tech-design/projects/keep/logic/adopt-libs-service-backup-snapshot-sink-keep-backup-verb.md

use std::time::SystemTime;

use keep::backup::{newest_backup_file, restore_to_data_dir};
use keep::backup::{
    run_backup, run_backup_once, sink_from_destination, snapshot_bytes, BackupDestination,
    KeepSnapshot, RetentionPolicy, KEEP_SNAPSHOT_VERSION,
};
use keep::persistence::recovery::RecoveryManager;
use keep::persistence::snapshot::SnapshotWriter;
use keep::storage::SnapshotConfig;
use keep::{KvEngine, KvKey, KvValue};
use std::path::Path;

/// R1 — `snapshot_bytes` captures the engine's live key/value set into a
/// versioned payload that deserializes back to the same values.
#[test]
fn snapshot_bytes_round_trips_engine_state() {
    let engine = KvEngine::new();
    engine
        .set(
            &KvKey::new("alpha").unwrap(),
            KvValue::String("one".into()),
            None,
        )
        .unwrap();
    engine
        .set(&KvKey::new("beta").unwrap(), KvValue::Int(2), None)
        .unwrap();

    let bytes = snapshot_bytes(&engine).unwrap();
    let snap: KeepSnapshot = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(snap.version, KEEP_SNAPSHOT_VERSION);
    assert_eq!(snap.values.len(), 2);
    assert!(snap
        .values
        .iter()
        .any(|(k, v)| k == "alpha" && *v == KvValue::String("one".into())));
    assert!(snap
        .values
        .iter()
        .any(|(k, v)| k == "beta" && *v == KvValue::Int(2)));
}

/// R2 — a `file://` destination writes an artifact via `run_backup_once`, and
/// `RetentionPolicy` age pruning (`BackupSink::prune`) removes the aged object.
#[test]
fn local_backup_writes_then_prunes() {
    let dir = tempfile::tempdir().unwrap();
    let uri = format!("file://{}", dir.path().display());
    let dest = BackupDestination::from_uri(&uri).unwrap();
    let sink = sink_from_destination(&dest).unwrap();

    let payload = br#"{"version":1,"values":[]}"#;
    let result = run_backup_once(
        sink.as_ref(),
        SystemTime::now(),
        payload,
        &RetentionPolicy::default(),
    )
    .unwrap();
    assert_eq!(result.object.bytes, payload.len());
    assert_eq!(result.pruned, 0, "default retention prunes nothing");
    let artifact = dir.path().join(&result.object.key);
    assert!(artifact.exists(), "backup artifact should be written");

    // Age pruning with max_age_seconds=0 removes every existing object.
    std::thread::sleep(std::time::Duration::from_millis(10));
    let removed = sink.prune(0).unwrap();
    assert_eq!(removed, 1, "retention prunes the aged artifact");
    assert!(!artifact.exists(), "pruned artifact is gone");
}

/// R1+R2 end-to-end — `run_backup` recovers a consistent snapshot from a data
/// dir (empty here) and writes it to a local destination, exactly as the
/// `keep backup` verb does.
#[test]
fn run_backup_end_to_end_local() {
    let data_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let dest =
        BackupDestination::from_uri(&format!("file://{}", dest_dir.path().display())).unwrap();

    let result = run_backup(data_dir.path(), 4, &dest, &RetentionPolicy::default()).unwrap();

    let artifact = dest_dir.path().join(&result.object.key);
    assert!(artifact.exists(), "run_backup writes an artifact");
    let snap: KeepSnapshot = serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    assert_eq!(snap.version, KEEP_SNAPSHOT_VERSION);
    assert!(
        snap.values.is_empty(),
        "an empty data dir yields an empty snapshot"
    );
}

/// `keep backup` fails, names the path and writes nothing when `--data-dir`
/// does not exist, instead of reporting an empty success.
#[test]
fn backup_verb_fails_when_data_dir_is_missing() {
    use std::process::Command;

    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-dir");
    let dest = dir.path().join("out");
    std::fs::create_dir(&dest).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_keep"))
        .args([
            "backup",
            "--dest",
            &format!("file://{}", dest.display()),
            "--data-dir",
            missing.to_str().unwrap(),
        ])
        .env_remove("KEEP_DATA_DIR")
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "keep backup must fail on a missing data dir"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr must name the missing path: {stderr}"
    );
    assert_eq!(
        std::fs::read_dir(&dest).unwrap().count(),
        0,
        "no artifact may be written"
    );
}

/// Three keys with the values the restore tests compare against.
fn three_keys() -> KvEngine {
    let engine = KvEngine::with_shards(4);
    engine
        .set(
            &KvKey::new("alpha").unwrap(),
            KvValue::String("one".into()),
            None,
        )
        .unwrap();
    engine
        .set(&KvKey::new("beta").unwrap(), KvValue::Int(2), None)
        .unwrap();
    engine
        .set(
            &KvKey::new("gamma").unwrap(),
            KvValue::String("three".into()),
            None,
        )
        .unwrap();
    engine
}

/// The engine's key/value pairs, sorted by key.
fn sorted_pairs(engine: &KvEngine) -> Vec<(String, KvValue)> {
    let mut pairs = engine.dump_values();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

fn file_uri(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

/// Writes `dest/backup-100.json` for [`three_keys`] and returns its sorted pairs.
fn seeded_backup(dest: &Path) -> Vec<(String, KvValue)> {
    let engine = three_keys();
    std::fs::write(
        dest.join("backup-100.json"),
        snapshot_bytes(&engine).unwrap(),
    )
    .unwrap();
    sorted_pairs(&engine)
}

/// Restore acceptance — write keys, `run_backup` to a `file://` destination,
/// restore into a directory that does not exist yet, recover it: same keys and values.
#[test]
fn restore_round_trip_returns_the_same_keys_and_values() {
    let src = tempfile::tempdir().unwrap();
    let engine = three_keys();
    SnapshotWriter::new(SnapshotConfig::default())
        .create_snapshot(&engine, src.path(), 0)
        .unwrap();
    let dest = tempfile::tempdir().unwrap();
    let dest_uri = BackupDestination::from_uri(&file_uri(dest.path())).unwrap();
    run_backup(src.path(), 4, &dest_uri, &RetentionPolicy::default()).unwrap();

    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("data");
    let res = restore_to_data_dir(&file_uri(dest.path()), &out, 4, false).unwrap();
    assert_eq!(res.keys, 3);

    let (restored, _) = RecoveryManager::recover(&out, 4).unwrap();
    assert_eq!(sorted_pairs(&restored), sorted_pairs(&engine));
}

/// The newest `<prefix>-<seconds>.json` wins over older artifacts and over non-artifact files.
#[test]
fn restore_picks_the_newest_artifact() {
    let dest = tempfile::tempdir().unwrap();
    let old = KvEngine::with_shards(4);
    old.set(&KvKey::new("old").unwrap(), KvValue::Int(1), None)
        .unwrap();
    std::fs::write(
        dest.path().join("backup-100.json"),
        snapshot_bytes(&old).unwrap(),
    )
    .unwrap();
    let new = KvEngine::with_shards(4);
    new.set(&KvKey::new("new").unwrap(), KvValue::Int(2), None)
        .unwrap();
    std::fs::write(
        dest.path().join("backup-200.json"),
        snapshot_bytes(&new).unwrap(),
    )
    .unwrap();
    std::fs::write(dest.path().join("notes.txt"), "x").unwrap();

    assert_eq!(
        newest_backup_file(&file_uri(dest.path())).unwrap(),
        dest.path().join("backup-200.json")
    );

    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("data");
    restore_to_data_dir(&file_uri(dest.path()), &out, 4, false).unwrap();
    let (restored, _) = RecoveryManager::recover(&out, 4).unwrap();
    let keys: Vec<String> = sorted_pairs(&restored)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(keys, vec!["new".to_string()]);
}

/// A non-empty data directory is refused without `force`; with `force` its files are replaced.
#[test]
fn restore_refuses_a_non_empty_data_dir_without_force() {
    let dest = tempfile::tempdir().unwrap();
    let expected = seeded_backup(dest.path());
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("keep.txt"), "x").unwrap();

    let err = restore_to_data_dir(&file_uri(dest.path()), out.path(), 4, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("is not empty"),
        "unexpected error: {err:#}"
    );
    assert!(out.path().join("keep.txt").exists());
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1);

    restore_to_data_dir(&file_uri(dest.path()), out.path(), 4, true).unwrap();
    assert!(!out.path().join("keep.txt").exists());
    let (restored, _) = RecoveryManager::recover(out.path(), 4).unwrap();
    assert_eq!(sorted_pairs(&restored), expected);
}

/// A corrupt artifact fails with a clear error and leaves the data directory empty.
#[test]
fn restore_fails_on_a_corrupt_artifact_and_leaves_the_data_dir_empty() {
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(dest.path().join("backup-100.json"), "not json").unwrap();
    let out = tempfile::tempdir().unwrap();

    let err = restore_to_data_dir(&file_uri(dest.path()), out.path(), 4, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("is corrupt"),
        "unexpected error: {err:#}"
    );
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
}

/// A wrong-version artifact fails with a clear error and leaves the data directory empty.
#[test]
fn restore_fails_on_a_wrong_version_artifact_and_leaves_the_data_dir_empty() {
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(
        dest.path().join("backup-100.json"),
        r#"{"version":99,"values":[]}"#,
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();

    let err = restore_to_data_dir(&file_uri(dest.path()), out.path(), 4, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("unsupported snapshot version 99"),
        "unexpected error: {err:#}"
    );
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
}

/// Restore reads `file://` sources only.
#[test]
fn restore_rejects_a_cloud_source() {
    let out = tempfile::tempdir().unwrap();
    let err = restore_to_data_dir("s3://bucket/prefix", out.path(), 4, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("only file:// sources"),
        "unexpected error: {err:#}"
    );
}

/// The `keep restore` verb: first run succeeds and prints the count; a second run on the
/// now non-empty directory is refused.
#[test]
fn restore_verb_round_trips_through_the_binary() {
    let dest = tempfile::tempdir().unwrap();
    let expected = seeded_backup(dest.path());
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("data");
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_keep"))
            .args([
                "restore",
                "--source",
                &file_uri(dest.path()),
                "--data-dir",
                out.to_str().unwrap(),
                "--shards",
                "4",
            ])
            .env_remove("KEEP_DATA_DIR")
            .env_remove("KEEP_SHARDS")
            .output()
            .unwrap()
    };

    let first = run();
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(json["keys"], 3);
    let (restored, _) = RecoveryManager::recover(&out, 4).unwrap();
    assert_eq!(sorted_pairs(&restored), expected);

    let second = run();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("is not empty"));
}
// HANDWRITE-END
