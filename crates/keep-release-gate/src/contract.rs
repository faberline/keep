//! The App table and the contract error of verify-release-contract (ported from the Python script).

use std::fmt;
use std::path::PathBuf;

/// A refused contract check. Its text is the line the Python script printed, without a prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError(pub String);

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ContractError {}

pub type Result<T> = std::result::Result<T, ContractError>;

/// Refuse with `message`.
pub fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(ContractError(message.into()))
}

pub const TWO_TARGETS: [&str; 2] = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];
pub const HARNESS_FIELDS: [&str; 3] = ["readyz", "round_trip", "durability"];

/// One row of the APPS table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub name: String,
    pub root: String,
    pub scripts_dir: String,
    pub shared_scripts: bool,
    pub onboarded: bool,
    pub targets: Vec<String>,
    pub candidate_jobs: Vec<String>,
    pub promotion_prefix: String,
    pub takes_attempt: bool,
    pub manifest_schema: String,
    pub receipt_name: String,
    pub receipt_schema: String,
    pub gke_backend: String,
    pub functional: Vec<String>,
}

impl App {
    /// The workflow inputs of the promotion workflow, in order.
    pub fn promotion_inputs(&self) -> Vec<String> {
        let mut inputs = vec!["version".to_string(), "candidate_run_id".to_string()];
        if self.takes_attempt {
            inputs.push("candidate_run_attempt".to_string());
        }
        let prefix = &self.promotion_prefix;
        inputs.push(format!("{prefix}_b64"));
        inputs.push(format!("{prefix}_sha256"));
        inputs.push(format!("{prefix}_sidecar_b64"));
        inputs.push(format!("{prefix}_sidecar_sha256"));
        inputs
    }

    /// The candidate workflow, relative to the repository root.
    pub fn candidate_workflow(&self) -> PathBuf {
        PathBuf::from(".github/workflows").join(format!("{}-release-candidate.yml", self.name))
    }

    /// The promotion workflow, relative to the repository root.
    pub fn promotion_workflow(&self) -> PathBuf {
        PathBuf::from(".github/workflows").join(format!("{}-release.yml", self.name))
    }
}

/// The job ids that every app's candidate workflow has, with the app's gates job.
pub fn shared_jobs(app: &str) -> Vec<String> {
    [
        "identity",
        "build",
        &format!("{app}-release-gates"),
        "ghcr-image-and-attest",
        "manifest",
        "verify-candidate",
        "result",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A row of the shared app shape.
pub fn shared_app(
    name: &str,
    root: &str,
    targets: &[&str],
    backend: &str,
    functional: &[&str],
    onboarded: bool,
) -> App {
    App {
        name: name.to_string(),
        root: root.to_string(),
        scripts_dir: "scripts/release".to_string(),
        shared_scripts: true,
        onboarded,
        targets: targets.iter().map(|s| s.to_string()).collect(),
        candidate_jobs: shared_jobs(name),
        promotion_prefix: "gke_receipt".to_string(),
        takes_attempt: true,
        manifest_schema: format!("cclab.{name}.candidate-manifest.v1"),
        receipt_name: format!("{name}-gke-receipt.json"),
        receipt_schema: format!("{name}.gke-release-receipt/v1"),
        gke_backend: backend.to_string(),
        functional: functional.iter().map(|s| s.to_string()).collect(),
    }
}

/// The APPS table: one row, `keep`.
pub fn apps() -> Vec<App> {
    vec![shared_app(
        "keep",
        ".",
        &TWO_TARGETS,
        "gke-acceptance",
        &HARNESS_FIELDS,
        true,
    )]
}

/// The row named `name`, or a refusal that lists the known apps.
pub fn find_app(name: &str) -> Result<App> {
    let rows = apps();
    if let Some(app) = rows.iter().find(|app| app.name == name) {
        return Ok(app.clone());
    }
    let known: Vec<&str> = rows.iter().map(|app| app.name.as_str()).collect();
    fail(format!("unknown app: {name} (known: {})", known.join(", ")))
}
