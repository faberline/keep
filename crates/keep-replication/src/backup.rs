// HANDWRITE-BEGIN gap="missing-generator:logic:a80dd388" tracker="pending-tracker" reason="keep's backup adoption: re-export service_backup types (mirrors lumen's backup_sink), a KeepSnapshot payload built from engine.dump_values, snapshot_bytes/snapshot_bytes_from_data_dir, and run_backup that recovers the engine, serializes a consistent snapshot, and calls sink_from_destination + run_backup_once."
//! keep backup adoption of the shared `libs/service-backup` contract.
//!
//! keep owns snapshot consistency: [`RecoveryManager`] rebuilds a full engine
//! from the on-disk snapshot + WAL, and [`KeepSnapshot`] serializes that
//! engine's live key/value set into one coherent payload. Destination / policy /
//! sink / runner primitives live in `libs/service-backup` so keep, lumen, relay,
//! and loom share a single backup contract instead of each carrying a bespoke
//! sink (this mirrors lumen's `backup_sink` re-export module).
//!
//! The `keep backup` CLI verb and the operator-rendered backup CronJob both
//! drive [`run_backup`]: recover → serialize → `sink_from_destination` →
//! `run_backup_once`. Cloud sinks (`s3://` / `gs://`) fail loud until a cloud
//! adapter feature is linked into `service-backup`; `file://` is the must-work
//! local path.
//!
//! @spec .aw/tech-design/projects/keep/logic/adopt-libs-service-backup-snapshot-sink-keep-backup-verb.md

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use keep_keyspace::engine::KvEngine;
use keep_keyspace::persistence::recovery::RecoveryManager;
use keep_keyspace::persistence::snapshot::SnapshotWriter;
use keep_shared_kernel::types::KvValue;
use keep_storage::SnapshotConfig;

pub use service_backup::{
    run_backup_once, sink_from_destination, BackupDestination, BackupObject, BackupPolicy,
    BackupRunResult, BackupSink, LocalFsSink, RetentionPolicy, UnsupportedCloudSink,
};

/// Snapshot payload format version (bump on an incompatible layout change).
pub const KEEP_SNAPSHOT_VERSION: u32 = 1;

/// A consistent, self-describing snapshot of keep's full key/value state.
///
/// TTL metadata is intentionally dropped — same as [`KvEngine::dump_values`]; a
/// restore re-imports values without expiry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeepSnapshot {
    /// Payload format version.
    pub version: u32,
    /// Every live (non-expired) key→value pair across all shards.
    pub values: Vec<(String, KvValue)>,
}

impl KeepSnapshot {
    /// Build a snapshot from a live engine (R1).
    pub fn from_engine(engine: &KvEngine) -> Self {
        Self {
            version: KEEP_SNAPSHOT_VERSION,
            values: engine.dump_values(),
        }
    }
}

/// Serialize a consistent snapshot of `engine` to JSON bytes (R1).
pub fn snapshot_bytes(engine: &KvEngine) -> Result<Vec<u8>> {
    serde_json::to_vec(&KeepSnapshot::from_engine(engine)).context("serialize keep snapshot")
}

/// Recover keep's on-disk state (latest snapshot + full WAL replay) into an
/// engine, then serialize a consistent full-state snapshot payload. This is the
/// self-contained source of bytes for the `keep backup` verb / CronJob runner,
/// so it never needs a live server.
pub fn snapshot_bytes_from_data_dir(data_dir: impl AsRef<Path>, shards: usize) -> Result<Vec<u8>> {
    let (engine, _stats) =
        RecoveryManager::recover(data_dir, shards).context("recover engine from data dir")?;
    snapshot_bytes(&engine)
}

/// One backup run (R2): recover a consistent snapshot from `data_dir`, write it
/// to `dest`, and apply `retention`. `file://` is the must-work local path;
/// `s3://` / `gs://` route to the shared fail-loud placeholder sink until a
/// cloud adapter feature is linked.
pub fn run_backup(
    data_dir: impl AsRef<Path>,
    shards: usize,
    dest: &BackupDestination,
    retention: &RetentionPolicy,
) -> Result<BackupRunResult> {
    let payload = snapshot_bytes_from_data_dir(data_dir, shards)?;
    let sink = sink_from_destination(dest)?;
    run_backup_once(sink.as_ref(), SystemTime::now(), &payload, retention)
}

/// Result of `keep restore`: the artifact that was read and what was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    /// Path of the backup artifact that was restored.
    pub source: String,
    /// Number of key/value pairs written.
    pub keys: usize,
    /// File name of the snapshot written into the data directory.
    pub snapshot_file: String,
}

/// Find the backup artifact named by `source` (`file://` only).
///
/// A `file://` path to a file is that artifact. A `file://` path to a directory
/// is scanned for `<prefix>-<digits>.json` files; the largest number wins.
pub fn newest_backup_file(source: &str) -> Result<PathBuf> {
    let Some(rest) = source.strip_prefix("file://") else {
        anyhow::bail!("restore supports only file:// sources, got `{source}`");
    };
    let path = PathBuf::from(rest);
    if path.is_file() {
        return Ok(path);
    }
    if path.is_dir() {
        let mut newest: Option<(u64, PathBuf)> = None;
        let entries = std::fs::read_dir(&path)
            .with_context(|| format!("read backup directory {}", path.display()))?;
        for entry in entries {
            let candidate = entry?.path();
            if !candidate.is_file() {
                continue;
            }
            let Some(name) = candidate.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Some((_, digits)) = stem.rsplit_once('-') else {
                continue;
            };
            let Ok(number) = digits.parse::<u64>() else {
                continue;
            };
            match &newest {
                Some((best, _)) if *best >= number => {}
                _ => newest = Some((number, candidate)),
            }
        }
        return newest
            .map(|(_, file)| file)
            .ok_or_else(|| anyhow::anyhow!("no backup artifact found in {}", path.display()));
    }
    anyhow::bail!("backup source {} does not exist", path.display())
}

/// Read and validate one backup artifact.
pub fn read_snapshot_file(path: &Path) -> Result<KeepSnapshot> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read backup artifact {}", path.display()))?;
    let snap: KeepSnapshot = serde_json::from_slice(&bytes)
        .with_context(|| format!("backup artifact {} is corrupt", path.display()))?;
    if snap.version != KEEP_SNAPSHOT_VERSION {
        anyhow::bail!(
            "backup artifact {} has unsupported snapshot version {} (this keep reads version {})",
            path.display(),
            snap.version,
            KEEP_SNAPSHOT_VERSION
        );
    }
    Ok(snap)
}

/// Load the newest backup artifact of `source` into `data_dir`, which must be
/// empty unless `force` is set (then its files are removed first).
///
/// Everything that can fail on the input is checked before the data directory
/// is changed: a corrupt or wrong-version artifact leaves it untouched.
pub fn restore_to_data_dir(
    source: &str,
    data_dir: &Path,
    shards: usize,
    force: bool,
) -> Result<RestoreResult> {
    let file = newest_backup_file(source)?;
    let snap = read_snapshot_file(&file)?;

    if data_dir.exists() {
        if !data_dir.is_dir() {
            anyhow::bail!("data directory {} is not a directory", data_dir.display());
        }
        let entries = std::fs::read_dir(data_dir)
            .with_context(|| format!("read data directory {}", data_dir.display()))?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<std::io::Result<Vec<PathBuf>>>()?;
        if !entries.is_empty() {
            if !force {
                anyhow::bail!(
                    "data directory {} is not empty; pass --force to replace its files",
                    data_dir.display()
                );
            }
            if entries.iter().any(|p| p.is_dir()) {
                anyhow::bail!(
                    "data directory {} contains a directory; refusing to remove it",
                    data_dir.display()
                );
            }
            for p in &entries {
                std::fs::remove_file(p).with_context(|| format!("remove {}", p.display()))?;
            }
        }
    }

    let keys = snap.values.len();
    let engine = KvEngine::with_shards(shards);
    engine.load_values(snap.values);
    let written =
        SnapshotWriter::new(SnapshotConfig::default()).create_snapshot(&engine, data_dir, 0)?;
    Ok(RestoreResult {
        source: file.display().to_string(),
        keys,
        snapshot_file: written
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    })
}
// HANDWRITE-END
