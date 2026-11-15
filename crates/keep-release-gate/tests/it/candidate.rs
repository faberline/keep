use keep_release_gate::candidate::{validate_candidate, CandidateSummary};
use keep_release_gate::fixtures::{self, FIXTURE_COMMIT};
use keep_release_gate::refusal::Result;
use keep_release_gate::util::sha256_hex;
use serde_json::{Map, Value};

/// Build the fixture candidate, apply `mutate`, and validate the serialized bytes.
fn check(mutate: impl FnOnce(&mut Value)) -> (Vec<u8>, Result<CandidateSummary>) {
    let mut candidate = fixtures::candidate("keep", "0.4.13", FIXTURE_COMMIT, "12345", "1");
    mutate(&mut candidate);
    let raw = serde_json::to_vec(&candidate).expect("serialize candidate");
    let map: Map<String, Value> = serde_json::from_slice(&raw).expect("parse candidate");
    let result = validate_candidate("keep", &raw, &map);
    (raw, result)
}

fn refusal(mutate: impl FnOnce(&mut Value)) -> String {
    check(mutate).1.unwrap_err().to_string()
}

#[test]
fn passing_candidate_is_summarized() {
    let (raw, result) = check(|_| {});
    let summary = result.expect("fixture candidate must pass");
    assert_eq!(summary.manifest_sha256, sha256_hex(&raw));
    assert_eq!(summary.repository, "faberline/keep");
    assert_eq!(summary.version, "0.4.13");
    assert_eq!(summary.commit, FIXTURE_COMMIT);
    assert_eq!(summary.run_id, "12345");
    assert_eq!(summary.run_attempt, "1");
    assert_eq!(
        summary.workflow_ref,
        "faberline/keep/.github/workflows/keep-release-candidate.yml@refs/heads/main"
    );
    assert!(summary.root_digest.starts_with("sha256:"));
    let value = summary.to_value();
    let object = value.as_object().expect("summary is an object");
    assert_eq!(object.len(), 10);
    for key in [
        "repository",
        "version",
        "commit",
        "run_id",
        "run_attempt",
        "workflow_ref",
        "manifest_sha256",
        "root_digest",
        "amd64_digest",
        "arm64_digest",
    ] {
        assert!(object.contains_key(key), "missing key {key}");
    }
}

#[test]
fn failed_job_is_refused() {
    let message = refusal(|c| c["jobs"]["verify-candidate"] = "failure".into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate does not bind every required successful job"
    );
}

#[test]
fn missing_job_is_refused() {
    let message = refusal(|c| {
        c["jobs"]
            .as_object_mut()
            .expect("jobs")
            .remove("keep-release-gates");
    });
    assert_eq!(
        message,
        "GKE receipt refused: candidate does not bind every required successful job"
    );
}

#[test]
fn root_equal_amd64_is_refused() {
    let message = refusal(|c| {
        let amd64 = c["image"]["amd64_digest"].clone();
        c["image"]["root_digest"] = amd64;
    });
    assert_eq!(
        message,
        "GKE receipt refused: candidate root and child digests must be distinct"
    );
}

#[test]
fn foreign_image_repository_is_refused() {
    let message = refusal(|c| c["image"]["repository"] = "ghcr.io/faberline/relay".into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate image repository changed"
    );
}

#[test]
fn tag_mismatch_is_refused() {
    let message = refusal(|c| c["tag"] = "keep@0.4.14".into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate release tag changed"
    );
}

#[test]
fn branch_source_is_refused() {
    let message = refusal(|c| c["source_ref"] = "refs/heads/feature".into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate was not built from main"
    );
}

#[test]
fn unscoped_candidate_tag_is_refused() {
    let message = refusal(|c| c["candidate_tag"] = "release-candidate-1-1".into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate image tag is not run scoped"
    );
}

#[test]
fn extra_target_is_refused() {
    let message = refusal(|c| {
        let mut extra = c["artifacts"][0].clone();
        extra["target"] = "aarch64-apple-darwin".into();
        c["artifacts"]
            .as_array_mut()
            .expect("artifacts")
            .push(extra);
    });
    assert_eq!(
        message,
        "GKE receipt refused: candidate artifact targets changed"
    );
}

#[test]
fn changed_digest_is_refused() {
    let short = format!("sha256:{}", "a".repeat(63));
    let message = refusal(|c| c["image"]["amd64_digest"] = short.into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate amd64 digest is not an immutable SHA-256 digest"
    );
}

#[test]
fn uppercase_digest_is_refused() {
    let upper = format!("sha256:{}", "A".repeat(64));
    let message = refusal(|c| c["image"]["root_digest"] = upper.into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate root digest is not an immutable SHA-256 digest"
    );
}

#[test]
fn extra_key_is_refused() {
    let message = refusal(|c| c["extra"] = "x".into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate manifest keys changed"
    );
}

#[test]
fn wrong_schema_is_refused() {
    let message = refusal(|c| c["schema"] = "cclab.relay.candidate-manifest.v1".into());
    assert_eq!(
        message,
        "GKE receipt refused: candidate manifest schema changed"
    );
}

#[test]
fn bad_version_commit_run_are_refused() {
    let version = refusal(|c| c["version"] = "0.4".into());
    assert_eq!(version, "GKE receipt refused: candidate version is invalid");

    let commit = refusal(|c| c["commit"] = "0".repeat(39).into());
    assert_eq!(commit, "GKE receipt refused: candidate commit is invalid");

    let run_id = refusal(|c| c["run_id"] = "12a".into());
    assert_eq!(run_id, "GKE receipt refused: candidate run ID is invalid");
}

#[test]
fn unknown_app_is_refused() {
    let message = validate_candidate("loom", b"{}", &Map::new())
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "GKE receipt refused: unknown shared-script app: loom (known: keep)"
    );
}
