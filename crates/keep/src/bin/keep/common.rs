//! Helpers shared by the `keep` CLI verb modules.

use anyhow::Result;

/// This binary's identity + build provenance for the standard CLI ops
/// (`upgrade` / `issue`), per the CONTRIBUTING.md CLI convention.
pub(crate) const TOOL: cli_std::ToolInfo = cli_std::ToolInfo {
    project: "keep",
    repo: "faberline/keep",
    target: env!("KEEP_TARGET"),
    version: env!("CARGO_PKG_VERSION"),
    git_sha: env!("KEEP_GIT_SHA"),
    built_at: env!("KEEP_BUILT_AT"),
};

/// Write `body` to `out` (a file, or `default_file` inside a directory) or print
/// it to stdout.
pub(crate) fn write_or_print(
    out: Option<&std::path::Path>,
    default_file: &str,
    body: &str,
) -> Result<()> {
    cli_std::artifact::write_or_print(out, default_file, body)?;
    Ok(())
}
