//! `keep k8s` — cluster artifacts split by lifecycle layer (CRD, operator, instance).

use std::path::PathBuf;

use anyhow::Result;

use clap::Subcommand;

use crate::common::write_or_print;

/// `keep k8s <crd|operator|instance>` — cluster artifacts split by lifecycle
/// layer.
#[derive(clap::Args, Debug)]
pub(crate) struct K8sArgs {
    #[command(subcommand)]
    cmd: K8sCmd,
}

#[derive(Subcommand, Debug)]
enum K8sCmd {
    /// Cluster-scoped API layer: render the Keep CRD.
    Crd(K8sCrdArgs),
    /// Operator control-plane layer: render assets or run the controller.
    Operator(K8sOperatorArgs),
    /// App-namespace declaration: render a Keep custom resource.
    Instance(K8sInstanceArgs),
}

#[derive(clap::Args, Debug)]
struct K8sCrdArgs {
    #[command(subcommand)]
    cmd: K8sCrdCmd,
}

#[derive(Subcommand, Debug)]
enum K8sCrdCmd {
    /// Render the Keep CustomResourceDefinition YAML.
    Render(K8sFileOutputArgs),
}

#[derive(clap::Args, Debug)]
struct K8sOperatorArgs {
    #[command(subcommand)]
    cmd: Option<K8sOperatorCmd>,
}

#[derive(Subcommand, Debug)]
enum K8sOperatorCmd {
    /// Container entrypoint: run the reconcile controller (needs `--features
    /// operator`). The default when no subcommand is given.
    Run,
    /// Render operator namespace/RBAC/deployment YAML.
    Render(K8sOperatorRenderArgs),
}

#[derive(clap::Args, Debug)]
struct K8sOperatorRenderArgs {
    /// Namespace that owns the operator control plane.
    #[arg(long, default_value = "keep-system")]
    namespace: String,
    /// Write to this path instead of stdout. A directory receives
    /// `operator.yaml`.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct K8sInstanceArgs {
    #[command(subcommand)]
    cmd: K8sInstanceCmd,
}

#[derive(Subcommand, Debug)]
enum K8sInstanceCmd {
    /// Render a namespaced `kind: Keep` custom resource.
    Render(K8sInstanceRenderArgs),
}

#[derive(clap::Args, Debug)]
struct K8sInstanceRenderArgs {
    /// Built-in instance profile.
    #[arg(long, value_enum, default_value_t = K8sInstanceProfile::Dev)]
    profile: K8sInstanceProfile,
    /// Keep CR name.
    #[arg(long)]
    name: Option<String>,
    /// Namespace where the app-facing Keep instance lives.
    #[arg(long)]
    namespace: Option<String>,
    /// Store image. Defaults are profile-specific.
    #[arg(long)]
    image: Option<String>,
    /// Write to this path instead of stdout. A directory receives `keep.yaml`.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum K8sInstanceProfile {
    /// Small local/kind CR: one store pod, small disk, verbose logs.
    Dev,
    /// Pre-prod CR: prod-shaped single node, json-ish info logs, mid disk.
    Staging,
    /// Production-shape CR: sharded raft-HA topology, large disk tier.
    Prod,
    /// Fill-in-the-blanks CR skeleton for app teams.
    Template,
}

#[derive(clap::Args, Debug)]
struct K8sFileOutputArgs {
    /// Write to this path instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// `keep k8s` — cluster artifacts split by lifecycle layer. Only `operator run`
/// needs kube-rs at runtime; the render paths are offline and work from the
/// binary (the generated CRD is embedded, the operator/instance manifests are
/// string-templated).
pub(crate) async fn run(args: K8sArgs) -> Result<()> {
    match args.cmd {
        K8sCmd::Crd(a) => match a.cmd {
            K8sCrdCmd::Render(a) => write_or_print(a.out.as_deref(), "crd.yaml", &crd_yaml()),
        },
        K8sCmd::Operator(a) => match a.cmd.unwrap_or(K8sOperatorCmd::Run) {
            K8sOperatorCmd::Run => run_operator().await,
            K8sOperatorCmd::Render(a) => {
                let yaml = render_operator_yaml(&a.namespace);
                write_or_print(a.out.as_deref(), "operator.yaml", &yaml)
            }
        },
        K8sCmd::Instance(a) => match a.cmd {
            K8sInstanceCmd::Render(a) => {
                let yaml = render_instance_yaml(&a);
                write_or_print(a.out.as_deref(), "keep.yaml", &yaml)
            }
        },
    }
}

// <HANDWRITE gap="missing-generator:logic" tracker="#2414" reason="logic section in keep.rs is hand-written pending codegen support">
#[cfg(feature = "operator")]
async fn run_operator() -> Result<()> {
    // The operator has no flags of its own, so the telemetry contract arrives
    // entirely through env — the same variables the rendered Deployment sets.
    let log_format = match std::env::var("KEEP_LOG_FORMAT").as_deref() {
        Ok("json") => service_http::LogFormat::Json,
        Ok("pretty") | Err(_) => service_http::LogFormat::Pretty,
        Ok(value) => anyhow::bail!("KEEP_LOG_FORMAT must be `pretty` or `json`, got `{value}`"),
    };
    let config = service_http::HttpConfig::new(
        "127.0.0.1",
        0,
        std::env::var("KEEP_LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
        log_format,
        0,
        0,
        std::env::var("KEEP_OTLP_ENDPOINT")
            .ok()
            .filter(|endpoint| !endpoint.is_empty()),
    );
    let identity = service_http::ServiceIdentity::new("keep", env!("CARGO_PKG_VERSION"))?;
    service_http::init_tracing_with_identity(&config, &identity)?;
    keep::operator::run().await
}
// </HANDWRITE>

#[cfg(not(feature = "operator"))]
async fn run_operator() -> Result<()> {
    anyhow::bail!(
        "this keep build was compiled without operator support; rebuild with \
         `--features operator` (the published image includes it)"
    )
}

#[cfg(feature = "operator")]
fn crd_yaml() -> String {
    keep::operator::crd_yaml()
}

#[cfg(not(feature = "operator"))]
fn crd_yaml() -> String {
    cli_std::artifact::ensure_trailing_newline(include_str!("../../../../../k8s/operator/crd.yaml"))
}

/// Render the operator control-plane manifests (RBAC + Deployment) with the
/// namespace substituted, from the checked-in fixtures.
fn render_operator_yaml(namespace: &str) -> String {
    let mut out = String::new();
    out.push_str(&cli_std::artifact::replace_kubernetes_namespace(
        include_str!("../../../../../k8s/operator/rbac.yaml"),
        "keep-system",
        namespace,
    ));
    out.push_str("\n---\n");
    out.push_str(&cli_std::artifact::replace_kubernetes_namespace(
        include_str!("../../../../../k8s/operator/deployment.yaml"),
        "keep-system",
        namespace,
    ));
    cli_std::artifact::ensure_trailing_newline(&out)
}

/// Render a `kind: Keep` custom resource for the selected profile.
fn render_instance_yaml(args: &K8sInstanceRenderArgs) -> String {
    let default_version = env!("CARGO_PKG_VERSION");
    let (default_name, default_namespace, default_image, body) = match args.profile {
        K8sInstanceProfile::Dev => (
            "keep",
            "default",
            "keep:latest".to_string(),
            InstanceBody::Dev,
        ),
        K8sInstanceProfile::Staging => (
            "keep",
            "staging",
            format!("keep:{default_version}"),
            InstanceBody::Staging,
        ),
        K8sInstanceProfile::Prod => (
            "keep",
            "production",
            format!("registry.example.com/keep:{default_version}"),
            InstanceBody::Prod,
        ),
        K8sInstanceProfile::Template => (
            "REPLACE_ME__KEEP_NAME",
            "REPLACE_ME__APP_NAMESPACE",
            "REPLACE_ME__REGISTRY/keep:REPLACE_ME__IMAGE_TAG".to_string(),
            InstanceBody::Template,
        ),
    };
    let name = args.name.as_deref().unwrap_or(default_name);
    let namespace = args.namespace.as_deref().unwrap_or(default_namespace);
    let image = args.image.as_deref().unwrap_or(&default_image);

    let mut yaml = format!(
        "apiVersion: keep.dev/v1alpha1\nkind: Keep\nmetadata:\n  name: {name}\n  namespace: {namespace}\nspec:\n  image: {image}\n"
    );
    match body {
        InstanceBody::Dev => {
            yaml.push_str("  shardCount: 1\n  replicasPerShard: 1\n  voterCount: 1\n  logLevel: debug\n  engineShards: 16\n  storage: 2Gi\n  resources:\n    cpu: \"250m\"\n    memory: 256Mi\n");
        }
        InstanceBody::Staging => {
            yaml.push_str("  shardCount: 1\n  replicasPerShard: 1\n  voterCount: 1\n  logLevel: info\n  engineShards: 128\n  storage: 20Gi\n  resources:\n    cpu: \"2\"\n    memory: 4Gi\n");
        }
        InstanceBody::Prod => {
            yaml.push_str("  imagePullPolicy: Always\n  shardCount: 3\n  replicasPerShard: 3\n  voterCount: 3\n  logLevel: info\n  engineShards: 512\n  storage: 200Gi\n  graceSecs: 45\n  resources:\n    cpu: \"8\"\n    memory: 16Gi\n");
        }
        InstanceBody::Template => {
            yaml.push_str("  imagePullPolicy: IfNotPresent\n  shardCount: REPLACE_ME__SHARD_COUNT\n  replicasPerShard: REPLACE_ME__REPLICAS_PER_SHARD\n  voterCount: REPLACE_ME__VOTER_COUNT\n  engineShards: 256\n  storage: 10Gi\n  resources:\n    cpu: \"2\"\n    memory: 4Gi\n");
        }
    }
    cli_std::artifact::ensure_trailing_newline(&yaml)
}

enum InstanceBody {
    Dev,
    Staging,
    Prod,
    Template,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    /// `keep k8s crd/operator/instance` verbs parse with their convention flags
    /// (#775). Positionals name subcommands; profile/namespace/out are flags.
    #[test]
    fn k8s_verbs_parse() {
        Cli::try_parse_from(["keep", "k8s", "crd", "render"]).expect("crd render");
        Cli::try_parse_from(["keep", "k8s", "operator", "run"]).expect("operator run");
        Cli::try_parse_from([
            "keep",
            "k8s",
            "operator",
            "render",
            "--namespace",
            "keep-system",
        ])
        .expect("operator render");

        let cli = Cli::try_parse_from([
            "keep",
            "k8s",
            "instance",
            "render",
            "--profile",
            "prod",
            "--namespace",
            "production",
        ])
        .expect("instance render");
        match cli.cmd {
            Some(Command::K8s(K8sArgs {
                cmd:
                    K8sCmd::Instance(K8sInstanceArgs {
                        cmd: K8sInstanceCmd::Render(a),
                    }),
            })) => {
                assert!(matches!(a.profile, K8sInstanceProfile::Prod));
                assert_eq!(a.namespace.as_deref(), Some("production"));
            }
            other => panic!("expected k8s instance render, got {other:?}"),
        }

        // `operator` with no subcommand defaults to `run`.
        let cli = Cli::try_parse_from(["keep", "k8s", "operator"]).expect("operator default");
        match cli.cmd {
            Some(Command::K8s(K8sArgs {
                cmd: K8sCmd::Operator(K8sOperatorArgs { cmd }),
            })) => assert!(cmd.is_none()),
            other => panic!("expected k8s operator, got {other:?}"),
        }
    }
}
