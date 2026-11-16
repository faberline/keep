//! The release candidate manifest, ported from `validate_candidate` of the receipt script.

use crate::refusal::{refuse, Result};
use crate::util::{is_ascii_digits, is_lower_hex, is_semver, require_digest, sha256_hex};
use serde_json::{json, Map, Value};

pub const REPOSITORY: &str = "faberline/keep";
pub const IMAGE_OWNER: &str = "ghcr.io/faberline";
pub const KEEP_TARGETS: [&str; 2] = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];

const CANDIDATE_KEYS: [&str; 18] = [
    "schema",
    "repository",
    "workflow_path",
    "workflow_id",
    "run_id",
    "run_attempt",
    "run_url",
    "source_ref",
    "workflow_ref",
    "commit",
    "version",
    "tag",
    "candidate_tag",
    "pr",
    "image",
    "artifacts",
    "sboms",
    "jobs",
];

const IMAGE_KEYS: [&str; 4] = ["repository", "root_digest", "amd64_digest", "arm64_digest"];

/// The release targets of `app`, in order. Only `keep` is known.
pub fn app_targets(app: &str) -> Result<&'static [&'static str]> {
    match app {
        "keep" => Ok(&KEEP_TARGETS[..]),
        other => refuse(format!("unknown shared-script app: {other} (known: keep)")),
    }
}

pub fn candidate_schema(app: &str) -> String {
    format!("cclab.{app}.candidate-manifest.v1")
}

pub fn receipt_schema(app: &str) -> String {
    format!("{app}.gke-release-receipt/v1")
}

pub fn workflow_ref(app: &str) -> String {
    format!("{REPOSITORY}/.github/workflows/{app}-release-candidate.yml@refs/heads/main")
}

/// The jobs that a successful candidate run must bind, each with the value `success`.
pub fn final_jobs(app: &str) -> Map<String, Value> {
    [
        "identity".to_string(),
        "build".to_string(),
        format!("{app}-release-gates"),
        "ghcr-image-and-attest".to_string(),
        "manifest".to_string(),
        "verify-candidate".to_string(),
        "result".to_string(),
    ]
    .into_iter()
    .map(|job| (job, Value::from("success")))
    .collect()
}

/// The checked fields of a candidate, and the hash of its manifest bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSummary {
    pub repository: String,
    pub version: String,
    pub commit: String,
    pub run_id: String,
    pub run_attempt: String,
    pub workflow_ref: String,
    pub manifest_sha256: String,
    pub root_digest: String,
    pub amd64_digest: String,
    pub arm64_digest: String,
}

impl CandidateSummary {
    pub fn to_value(&self) -> Value {
        json!({
            "repository": self.repository,
            "version": self.version,
            "commit": self.commit,
            "run_id": self.run_id,
            "run_attempt": self.run_attempt,
            "workflow_ref": self.workflow_ref,
            "manifest_sha256": self.manifest_sha256,
            "root_digest": self.root_digest,
            "amd64_digest": self.amd64_digest,
            "arm64_digest": self.arm64_digest,
        })
    }
}

/// A string field of `candidate`, if it is one.
fn text<'a>(candidate: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    candidate.get(key).and_then(Value::as_str)
}

/// Check a candidate manifest. The checks run in the same order, with the same messages, as the receipt script.
pub fn validate_candidate(
    app: &str,
    raw: &[u8],
    candidate: &Map<String, Value>,
) -> Result<CandidateSummary> {
    let targets = app_targets(app)?;
    if candidate.len() != CANDIDATE_KEYS.len()
        || !CANDIDATE_KEYS
            .iter()
            .all(|key| candidate.contains_key(*key))
    {
        return refuse("candidate manifest keys changed");
    }
    if text(candidate, "schema") != Some(candidate_schema(app).as_str()) {
        return refuse("candidate manifest schema changed");
    }
    if text(candidate, "repository") != Some(REPOSITORY) {
        return refuse("candidate repository changed");
    }
    let Some(version) = text(candidate, "version").filter(|v| is_semver(v)) else {
        return refuse("candidate version is invalid");
    };
    let Some(commit) = text(candidate, "commit").filter(|c| is_lower_hex(c, 40)) else {
        return refuse("candidate commit is invalid");
    };
    let Some(run_id) = text(candidate, "run_id").filter(|r| is_ascii_digits(r)) else {
        return refuse("candidate run ID is invalid");
    };
    let Some(run_attempt) = text(candidate, "run_attempt").filter(|r| is_ascii_digits(r)) else {
        return refuse("candidate run attempt is invalid");
    };
    if text(candidate, "tag") != Some(format!("{app}@{version}").as_str()) {
        return refuse("candidate release tag changed");
    }
    if text(candidate, "candidate_tag")
        != Some(format!("release-candidate-{run_id}-{run_attempt}").as_str())
    {
        return refuse("candidate image tag is not run scoped");
    }
    if text(candidate, "source_ref") != Some("refs/heads/main") {
        return refuse("candidate was not built from main");
    }
    if text(candidate, "workflow_path")
        != Some(format!(".github/workflows/{app}-release-candidate.yml").as_str())
    {
        return refuse("candidate workflow path changed");
    }
    if text(candidate, "workflow_ref") != Some(workflow_ref(app).as_str()) {
        return refuse("candidate workflow identity changed");
    }
    if candidate.get("jobs") != Some(&Value::Object(final_jobs(app))) {
        return refuse("candidate does not bind every required successful job");
    }
    let artifact_targets: Option<Vec<Option<&str>>> = candidate
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| item.get("target").and_then(Value::as_str))
                .collect()
        });
    let expected: Vec<Option<&str>> = targets.iter().map(|target| Some(*target)).collect();
    if artifact_targets.as_ref() != Some(&expected) {
        return refuse("candidate artifact targets changed");
    }
    let Some(image) = candidate.get("image").and_then(Value::as_object) else {
        return refuse("candidate image shape changed");
    };
    if image.len() != IMAGE_KEYS.len() || !IMAGE_KEYS.iter().all(|key| image.contains_key(*key)) {
        return refuse("candidate image shape changed");
    }
    let image_repository = format!("{IMAGE_OWNER}/{app}");
    if image.get("repository").and_then(Value::as_str) != Some(image_repository.as_str()) {
        return refuse("candidate image repository changed");
    }
    let root = require_digest(image.get("root_digest"), "candidate root digest")?;
    let amd64 = require_digest(image.get("amd64_digest"), "candidate amd64 digest")?;
    let arm64 = require_digest(image.get("arm64_digest"), "candidate arm64 digest")?;
    if root == amd64 || root == arm64 || amd64 == arm64 {
        return refuse("candidate root and child digests must be distinct");
    }
    Ok(CandidateSummary {
        repository: REPOSITORY.to_string(),
        version: version.to_string(),
        commit: commit.to_string(),
        run_id: run_id.to_string(),
        run_attempt: run_attempt.to_string(),
        workflow_ref: workflow_ref(app),
        manifest_sha256: sha256_hex(raw),
        root_digest: root,
        amd64_digest: amd64,
        arm64_digest: arm64,
    })
}
