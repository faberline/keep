//! `keep dockerfile` — render keep's runtime image Dockerfiles (offline).

use std::path::PathBuf;

use anyhow::Result;

use clap::{Subcommand, ValueEnum};

use crate::common::write_or_print;

/// `keep dockerfile <render>` — render keep's runtime image Dockerfiles.
///
/// @spec .aw/tech-design/projects/keep/interfaces/cli/deploy-cli-keep-spec-spec-gen-dockerfile-render.md
#[derive(clap::Args, Debug)]
pub(crate) struct DockerfileArgs {
    #[command(subcommand)]
    cmd: DockerfileCmd,
}

#[derive(Subcommand, Debug)]
enum DockerfileCmd {
    /// Render a Dockerfile to stdout or `--out`.
    Render(DockerfileRenderArgs),
}

#[derive(clap::Args, Debug)]
struct DockerfileRenderArgs {
    /// Which runtime image contract to render.
    #[arg(long, value_enum, default_value_t = DockerfileVariant::Source)]
    variant: DockerfileVariant,
    /// Release tag used by `--variant release`; accepts `0.4.3` or `keep@0.4.3`.
    #[arg(long)]
    version: Option<String>,
    /// Write to this path instead of stdout. A directory receives `Dockerfile`
    /// or `Dockerfile.release`.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DockerfileVariant {
    /// Build from the workspace source tree.
    Source,
    /// Fetch and verify a published `keep@<version>` release binary.
    Release,
}

/// `keep dockerfile render` — render keep's runtime image Dockerfiles. The
/// checked-in Dockerfiles are the fixtures; the CLI is their in-binary form
/// (marker stripping + `keep@version` substitution), so `render` stays the
/// source of truth.
///
/// @spec .aw/tech-design/projects/keep/interfaces/cli/deploy-cli-keep-spec-spec-gen-dockerfile-render.md
pub(crate) fn run(args: DockerfileArgs) -> Result<()> {
    match args.cmd {
        DockerfileCmd::Render(a) => {
            let (file_name, body) = match a.variant {
                DockerfileVariant::Source => ("Dockerfile", render_source_dockerfile()),
                DockerfileVariant::Release => (
                    "Dockerfile.release",
                    render_release_dockerfile(a.version.as_deref()),
                ),
            };
            write_or_print(a.out.as_deref(), file_name, &body)
        }
    }
}

fn render_source_dockerfile() -> String {
    cli_std::artifact::strip_source_ownership_markers(include_str!("../../../../../Dockerfile"))
}

fn render_release_dockerfile(version: Option<&str>) -> String {
    let tag = cli_std::artifact::release_tag("keep", version, env!("CARGO_PKG_VERSION"));
    let version = tag.trim_start_matches("keep@");
    let template = cli_std::artifact::strip_source_ownership_markers(include_str!(
        "../../../../../Dockerfile.release"
    ));
    let mut out = String::new();
    for line in template.lines() {
        if line.starts_with("#   docker build -f Dockerfile.release -t keep:") {
            out.push_str(&format!(
                "#   docker build -f Dockerfile.release -t keep:{version} \\"
            ));
        } else if line.starts_with("#     --build-arg KEEP_VERSION=") {
            out.push_str(&format!("#     --build-arg KEEP_VERSION={tag} ."));
        } else if line.starts_with("ARG KEEP_VERSION=") {
            out.push_str(&format!("ARG KEEP_VERSION={tag}"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    /// `keep dockerfile render` parses with its convention flags (#777).
    #[test]
    fn dockerfile_verbs_parse() {
        // dockerfile render.
        let cli = Cli::try_parse_from([
            "keep",
            "dockerfile",
            "render",
            "--variant",
            "release",
            "--version",
            "1.2.3",
        ])
        .expect("dockerfile render should parse");
        match cli.cmd {
            Some(Command::Dockerfile(DockerfileArgs {
                cmd: DockerfileCmd::Render(a),
            })) => {
                assert!(matches!(a.variant, DockerfileVariant::Release));
                assert_eq!(a.version.as_deref(), Some("1.2.3"));
            }
            other => panic!("expected dockerfile render, got {other:?}"),
        }
    }
}
