//! `keep restore` — load a backup artifact into a data directory (offline).

use std::path::PathBuf;

use anyhow::Result;

/// `keep restore --source <uri> --data-dir <dir>` flags.
#[derive(clap::Args, Debug)]
pub(crate) struct RestoreArgs {
    /// Backup source: `file:///dir` (the newest `<prefix>-<seconds>.json` in it) or `file:///dir/backup-123.json`.
    #[arg(long)]
    source: String,
    /// Data directory to restore into; created when missing.
    #[arg(long, env = "KEEP_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,
    /// Engine shard count — match the server's `--shards`.
    #[arg(long, env = "KEEP_SHARDS", default_value_t = 256)]
    shards: usize,
    /// Replace the files of a non-empty data directory.
    #[arg(long)]
    force: bool,
}

/// `keep restore` — load the newest backup artifact into a data directory.
pub(crate) fn run(args: RestoreArgs) -> Result<()> {
    let result =
        keep::backup::restore_to_data_dir(&args.source, &args.data_dir, args.shards, args.force)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
