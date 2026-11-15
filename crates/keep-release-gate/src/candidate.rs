//! The candidate manifest rules, ported from `make-gke-release-receipt.py`.

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

/// The release targets of `app`. Only `keep` is known.
pub fn app_targets(app: &str) -> Result<&'static [&'static str]> {
    match app {
        "keep" => Ok(&KEEP_TARGETS),
        _ => refuse(format!("unknown shared-script app: {app} (known: keep)")),
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

/// The seven jobs that must each report `success`.
pub fn final_jobs(app: &str) -> Map<String, Value> {
    let names = [
        "identity".to_string(),
        "build".to_string(),
        format!("{app}-release-gates"),
        "ghcr-image-and-attest".to_string(),
        "manifest".to_string(),
        "verify-candidate".to_string(),
        "result".to_string(),
    ];
    names
        .into_iter()
        .map(|job| (job, json!("success")))
        .collect()
}

/// The fields of a validated candidate, as the receipt records them.
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

/// True when `value` is the string `expected`.
fn is_str(value: Option<&Value>, expected: &str) -> bool {
    value.and_then(Value::as_str) == Some(expected)
}

/// True when `value` is a string that passes `check`.
fn str_where(value: Option<&Value>, check: impl Fn(&str) -> bool) -> bool {
    value.and_then(Value::as_str).is_some_and(check)
}

/// Validate a candidate manifest. `raw` are its bytes, `candidate` its parsed object.
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
    let get = |key: &str| candidate.get(key);
    if !is_str(get("schema"), &candidate_schema(app)) {
        return refuse("candidate manifest schema changed");
    }
    if !is_str(get("repository"), REPOSITORY) {
        return refuse("candidate repository changed");
    }
    if !str_where(get("version"), is_semver) {
        return refuse("candidate version is invalid");
    }
    if !str_where(get("commit"), |c| is_lower_hex(c, 40)) {
        return refuse("candidate commit is invalid");
    }
    if !str_where(get("run_id"), is_ascii_digits) {
        return refuse("candidate run ID is invalid");
    }
    if !str_where(get("run_attempt"), is_ascii_digits) {
        return refuse("candidate run attempt is invalid");
    }
    let version = get("version").and_then(Value::as_str).unwrap_or_default();
    let run_id = get("run_id").and_then(Value::as_str).unwrap_or_default();
    let run_attempt = get("run_attempt")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let commit = get("commit").and_then(Value::as_str).unwrap_or_default();

    if !is_str(get("tag"), &format!("{app}@{version}")) {
        return refuse("candidate release tag changed");
    }
    if !is_str(
        get("candidate_tag"),
        &format!("release-candidate-{run_id}-{run_attempt}"),
    ) {
        return refuse("candidate image tag is not run scoped");
    }
    if !is_str(get("source_ref"), "refs/heads/main") {
        return refuse("candidate was not built from main");
    }
    if !is_str(
        get("workflow_path"),
        &format!(".github/workflows/{app}-release-candidate.yml"),
    ) {
        return refuse("candidate workflow path changed");
    }
    if !is_str(get("workflow_ref"), &workflow_ref(app)) {
        return refuse("candidate workflow identity changed");
    }
    if get("jobs") != Some(&Value::Object(final_jobs(app))) {
        return refuse("candidate does not bind every required successful job");
    }
    let artifacts_match = get("artifacts")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.len() == targets.len()
                && list
                    .iter()
                    .zip(targets)
                    .all(|(artifact, target)| is_str(artifact.get("target"), target))
        });
    if !artifacts_match {
        return refuse("candidate artifact targets changed");
    }
    let image = match get("image").and_then(Value::as_object) {
        Some(image)
            if image.len() == 4
                && ["repository", "root_digest", "amd64_digest", "arm64_digest"]
                    .iter()
                    .all(|key| image.contains_key(*key)) =>
        {
            image
        }
        _ => return refuse("candidate image shape changed"),
    };
    if !is_str(image.get("repository"), &format!("{IMAGE_OWNER}/{app}")) {
        return refuse("candidate image repository changed");
    }
    let root = require_digest(image.get("root_digest"), "candidate root digest")?;
    let amd64 = require_digest(image.get("amd64_digest"), "candidate amd64 digest")?;
    let arm64 = require_digest(image.get("arm64_digest"), "candidate arm64 digest")?;
    if [&root, &amd64, &arm64]
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != 3
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_and_workflow_identity_follow_the_app() {
        assert_eq!(candidate_schema("keep"), "cclab.keep.candidate-manifest.v1");
        assert_eq!(receipt_schema("keep"), "keep.gke-release-receipt/v1");
        assert_eq!(
            workflow_ref("keep"),
            "faberline/keep/.github/workflows/keep-release-candidate.yml@refs/heads/main"
        );
        assert_eq!(app_targets("keep").expect("keep"), &KEEP_TARGETS[..]);
    }
}
