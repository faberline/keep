//! `keep upgrade` — self-update this binary from a published release.

use anyhow::Result;

use crate::common::TOOL;

/// `keep upgrade` flags.
#[derive(clap::Args, Debug)]
pub(crate) struct UpgradeArgs {
    /// Report the current and latest version without modifying the binary.
    #[arg(long)]
    check: bool,
    /// Install this exact version (`0.4.3` or `keep@0.4.3`) instead of the latest.
    #[arg(long)]
    tag: Option<String>,
    /// Reinstall even when already on the selected version.
    #[arg(long)]
    force: bool,
    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    yes: bool,
}

/// `keep upgrade` — self-update from a published release.
pub(crate) async fn run(args: UpgradeArgs) -> Result<()> {
    cli_std::upgrade::run(
        &TOOL,
        cli_std::upgrade::Options {
            check: args.check,
            tag: args.tag,
            force: args.force,
            yes: args.yes,
        },
    )
    .await
}
