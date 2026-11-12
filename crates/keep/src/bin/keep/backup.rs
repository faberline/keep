//! `keep backup` — write a consistent snapshot through the shared backup runner.

use std::path::PathBuf;

use anyhow::Result;

/// `keep backup --dest <uri>` flags (#776). The destination is the one primary
/// object; everything else is a flag (the CLI convention).
#[derive(clap::Args, Debug)]
pub(crate) struct BackupArgs {
    /// Destination URI: `file:///path`, `s3://bucket/prefix`, or
    /// `gs://bucket/prefix`.
    #[arg(long)]
    dest: String,
    /// Data directory to recover the snapshot from (WAL + snapshots); matches
    /// the server's `--data-dir`.
    #[arg(long, env = "KEEP_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,
    /// Engine shard count used to recover the data dir — match the server's
    /// `--shards`.
    #[arg(long, env = "KEEP_SHARDS", default_value_t = 256)]
    shards: usize,
    /// Drop backup objects older than this many seconds after a successful put
    /// (omit to keep everything).
    #[arg(long)]
    retention_secs: Option<u64>,
}

/// `keep backup` — recover a consistent snapshot from the data dir and write it
/// through the shared libs/service-backup runner (#776). Offline: no server, no
/// network beyond the sink (local `file://` here).
///
/// @spec .aw/tech-design/projects/keep/logic/adopt-libs-service-backup-snapshot-sink-keep-backup-verb.md
pub(crate) fn run(args: BackupArgs) -> Result<()> {
    if !args.data_dir.is_dir() {
        anyhow::bail!(
            "backup data directory `{}` does not exist or is not a directory; refusing to write an empty backup",
            args.data_dir.display()
        );
    }
    let dest = keep::backup::BackupDestination::from_uri(&args.dest)?;
    let retention = match args.retention_secs {
        Some(secs) => keep::backup::RetentionPolicy::max_age_seconds(secs),
        None => keep::backup::RetentionPolicy::default(),
    };
    let result = keep::backup::run_backup(&args.data_dir, args.shards, &dest, &retention)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
