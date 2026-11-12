//! keep server — HTTP/2 + OpenAPI front end over the sharded KV engine.
//!
//! Cloud-native: env-driven config, `/healthz` + `/readyz` probes, and
//! SIGTERM-aware graceful drain so k8s can roll pods without dropping requests.
//!
//! Bare `keep` (no subcommand) runs the server with the flags below; the
//! standard agent-facing commands — `keep llm`, `keep upgrade`, `keep issue`
//! (`search`/`view`/`create`) (the CONTRIBUTING.md CLI convention, via the
//! shared `cli-std` lib) — sit alongside it. Agents start at `keep llm
//! outline`.

#[path = "keep/backup.rs"]
mod backup;
#[path = "keep/common.rs"]
mod common;
#[path = "keep/dockerfile.rs"]
mod dockerfile;
#[path = "keep/issue.rs"]
mod issue;
#[path = "keep/k8s.rs"]
mod k8s;
#[path = "keep/llm.rs"]
mod llm;
#[path = "keep/restore.rs"]
mod restore;
#[path = "keep/serve.rs"]
mod serve;
#[path = "keep/serve_config.rs"]
mod serve_config;
#[path = "keep/spec.rs"]
mod spec;
#[path = "keep/upgrade.rs"]
mod upgrade;

use anyhow::Result;
use clap::{Parser, Subcommand};

use backup::BackupArgs;
use dockerfile::DockerfileArgs;
use issue::IssueArgs;
use k8s::K8sArgs;
use llm::LlmArgs;
use restore::RestoreArgs;
use serve::ServeArgs;
use spec::SpecArgs;
use upgrade::UpgradeArgs;

#[derive(Parser, Debug)]
#[command(
    name = "keep",
    version,
    about = "keep — cloud-native KV / claim-check store (HTTP/2 + OpenAPI)"
)]
struct Cli {
    /// Standard agent-facing command. Omit it to run the server (the default).
    #[command(subcommand)]
    cmd: Option<Command>,
    /// Server flags — used when no subcommand is given (`keep <flags>`).
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print agent-facing LLM topics — offline, no server. `outline` (default)
    /// maps the topics; pass a topic id for detail (`--format json` for a
    /// machine-readable form).
    Llm(LlmArgs),
    /// Self-update this binary from a published GitHub release. Resolves the
    /// running target + version, downloads the matching `keep-<target>.tar.gz`,
    /// verifies its sha256, and atomically replaces the executable. `--check`
    /// reports the available version without changing anything.
    Upgrade(UpgradeArgs),
    /// Search, view, file, and follow up on keep issues on the axiom tracker
    /// (`search`/`view`/`create`/`comment`). `create` bundles diagnostics and
    /// auto-tags `app:keep`; `comment` reopens before posting diagnostics.
    Issue(IssueArgs),
    /// Kubernetes artifacts split by layer: the cluster-scoped CRD, the operator
    /// control plane, and app-namespace Keep instances. Render paths are offline
    /// (they work from the binary); only `operator run` needs the `operator`
    /// build feature.
    K8s(K8sArgs),
    /// Write a consistent snapshot of keep's on-disk state to a backup
    /// destination through the shared libs/service-backup runner. Recovers the
    /// engine from `--data-dir` (latest snapshot + WAL replay), serializes a
    /// full-state payload, and stores it at `--dest` (`file://` local; `s3://`,
    /// `gs://` route to the shared placeholder sink until a cloud adapter
    /// feature is linked), applying optional age retention.
    Backup(BackupArgs),
    /// Load the newest backup artifact written by `keep backup` into a data directory (offline, no server). Refuses a non-empty `--data-dir` unless `--force` is given.
    Restore(RestoreArgs),
    /// Print keep's machine-readable integration spec — offline, no server. The
    /// default emits the OpenAPI 3 JSON document (the offline twin of
    /// `/openapi.json`); `--format openapi-yaml` for LLM-readable YAML,
    /// `--format json-schema` for the data types, `--shapes` for the request
    /// cookbook, `--fields` for the value-type catalog. `spec gen --lang
    /// ts|py|rust --out <dir>` generates a typed client instead.
    // @spec .aw/tech-design/projects/keep/interfaces/cli/deploy-cli-keep-spec-spec-gen-dockerfile-render.md
    Spec(SpecArgs),
    /// Render keep's runtime image Dockerfiles — offline, no server. Image
    /// construction is owned here (not by `k8s`) because the same artifact feeds
    /// compose, kind, and real registries.
    // @spec .aw/tech-design/projects/keep/interfaces/cli/deploy-cli-keep-spec-spec-gen-dockerfile-render.md
    Dockerfile(DockerfileArgs),
}

fn main() -> Result<()> {
    // Install the process-level rustls crypto provider before anything parses or
    // dials TLS (the operator/kube, raft-runtime peer, and online CLI paths all link
    // rustls, which panics without a default provider). A no-op in the default,
    // rustls-free build. See `keep::tls`.
    keep::tls::install_default_crypto_provider();
    let cli = Cli::parse();
    let worker_threads = if cli.cmd.is_none() && cli.serve.worker_threads > 0 {
        cli.serve.worker_threads
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    };
    let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
    rt_builder.worker_threads(worker_threads).enable_all();
    let rt = rt_builder.build()?;
    rt.block_on(async move {
        match cli.cmd {
            // Default (no subcommand): run the server.
            None => serve::serve_main(cli.serve, worker_threads).await,
            Some(cmd) => dispatch(cmd).await,
        }
    })
}

async fn dispatch(cmd: Command) -> Result<()> {
    match cmd {
        Command::Llm(args) => llm::run(args),
        Command::Upgrade(args) => upgrade::run(args).await,
        Command::Issue(args) => issue::run(args).await,
        Command::K8s(args) => k8s::run(args).await,
        Command::Backup(args) => backup::run(args),
        Command::Restore(args) => restore::run(args),
        Command::Spec(args) => spec::run(args),
        Command::Dockerfile(args) => dockerfile::run(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The clap derive surface is internally consistent (catches conflicting
    /// args / bad value parsers at test time, not at runtime).
    #[test]
    fn clap_command_is_valid() {
        Cli::command().debug_assert();
    }

    /// The migrated-away `keep report-issue` command no longer parses (#540).
    #[test]
    fn report_issue_command_is_gone() {
        assert!(Cli::try_parse_from(["keep", "report-issue", "--title", "x"]).is_err());
    }
}
