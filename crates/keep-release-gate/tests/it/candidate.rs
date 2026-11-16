//! Refusal tests for `validate_candidate`. Each refusal asserts the exact text.

use keep_release_gate::candidate::validate_candidate;
use keep_release_gate::fixtures::{candidate, FIXTURE_COMMIT};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// The fixture candidate after `mutate`: the bytes that are written, and the parsed object.
fn written(mutate: impl FnOnce(&mut Value)) -> (Vec<u8>, Map<String, Value>) {
    let mut value = candidate("keep", "0.4.13", FIXTURE_COMMIT, "12345", "1");
    mutate(&mut value);
    let bytes = serde_json::to_vec(&value).unwrap();
    let map = value.as_object().unwrap().clone();
    (bytes, map)
}

/// The refusal text of the fixture candidate after `mutate`.
fn refused(mutate: impl FnOnce(&mut Value)) -> String {
    let (bytes, map) = written(mutate);
    validate_candidate("keep", &bytes, &map)
        .unwrap_err()
        .to_string()
}

#[test]
fn passing_candidate_is_summarized() {
    let (bytes, map) = written(|_| {});
    let summary = validate_candidate("keep", &bytes, &map).unwrap();
    assert_eq!(
        summary.manifest_sha256,
        format!("{:x}", Sha256::digest(&bytes))
    );
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
    let object = value.as_object().unwrap();
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
        assert!(object.contains_key(key), "summary lacks {key}");
    }
}

#[test]
fn failed_job_is_refused() {
    assert_eq!(
        refused(|v| v["jobs"]["verify-candidate"] = "failure".into()),
        "GKE receipt refused: candidate does not bind every required successful job"
    );
}

#[test]
fn missing_job_is_refused() {
    assert_eq!(
        refused(|v| {
            v["jobs"]
                .as_object_mut()
                .unwrap()
                .remove("keep-release-gates");
        }),
        "GKE receipt refused: candidate does not bind every required successful job"
    );
}

#[test]
fn root_equal_to_amd64_is_refused() {
    assert_eq!(
        refused(|v| {
            let amd64 = v["image"]["amd64_digest"].clone();
            v["image"]["root_digest"] = amd64;
        }),
        "GKE receipt refused: candidate root and child digests must be distinct"
    );
}

#[test]
fn foreign_image_repository_is_refused() {
    assert_eq!(
        refused(|v| v["image"]["repository"] = "ghcr.io/faberline/relay".into()),
        "GKE receipt refused: candidate image repository changed"
    );
}

#[test]
fn tag_mismatch_is_refused() {
    assert_eq!(
        refused(|v| v["tag"] = "keep@0.4.14".into()),
        "GKE receipt refused: candidate release tag changed"
    );
}

#[test]
fn branch_source_is_refused() {
    assert_eq!(
        refused(|v| v["source_ref"] = "refs/heads/feature".into()),
        "GKE receipt refused: candidate was not built from main"
    );
}

#[test]
fn unscoped_candidate_tag_is_refused() {
    assert_eq!(
        refused(|v| v["candidate_tag"] = "release-candidate-1-1".into()),
        "GKE receipt refused: candidate image tag is not run scoped"
    );
}

#[test]
fn extra_target_is_refused() {
    assert_eq!(
        refused(|v| {
            let mut extra = v["artifacts"][0].clone();
            extra["target"] = "aarch64-apple-darwin".into();
            v["artifacts"].as_array_mut().unwrap().push(extra);
        }),
        "GKE receipt refused: candidate artifact targets changed"
    );
}

#[test]
fn changed_digest_is_refused() {
    assert_eq!(
        refused(|v| v["image"]["amd64_digest"] = format!("sha256:{}", "a".repeat(63)).into()),
        "GKE receipt refused: candidate amd64 digest is not an immutable SHA-256 digest"
    );
}

#[test]
fn uppercase_digest_is_refused() {
    assert_eq!(
        refused(|v| v["image"]["root_digest"] = format!("sha256:{}", "A".repeat(64)).into()),
        "GKE receipt refused: candidate root digest is not an immutable SHA-256 digest"
    );
}

#[test]
fn extra_key_is_refused() {
    assert_eq!(
        refused(|v| v["extra"] = "x".into()),
        "GKE receipt refused: candidate manifest keys changed"
    );
}

#[test]
fn wrong_schema_is_refused() {
    assert_eq!(
        refused(|v| v["schema"] = "cclab.relay.candidate-manifest.v1".into()),
        "GKE receipt refused: candidate manifest schema changed"
    );
}

#[test]
fn bad_version_commit_and_run_are_refused() {
    assert_eq!(
        refused(|v| v["version"] = "0.4".into()),
        "GKE receipt refused: candidate version is invalid"
    );
    assert_eq!(
        refused(|v| v["commit"] = FIXTURE_COMMIT[..39].into()),
        "GKE receipt refused: candidate commit is invalid"
    );
    assert_eq!(
        refused(|v| v["run_id"] = "12a".into()),
        "GKE receipt refused: candidate run ID is invalid"
    );
}

#[test]
fn unknown_app_is_refused() {
    let err = validate_candidate("loom", b"{}", &Map::new()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "GKE receipt refused: unknown shared-script app: loom (known: keep)"
    );
}

#[test]
fn remaining_checks_are_refused() {
    type Mutation = fn(&mut Value);
    let cases: Vec<(Mutation, &str)> = vec![
        (
            |v| v["repository"] = "faberline/relay".into(),
            "candidate repository changed",
        ),
        (
            |v| v["workflow_path"] = ".github/workflows/x.yml".into(),
            "candidate workflow path changed",
        ),
        (
            |v| v["workflow_ref"] = "x".into(),
            "candidate workflow identity changed",
        ),
        (
            |v| v["artifacts"] = json!("x"),
            "candidate artifact targets changed",
        ),
        (
            |v| v["artifacts"] = json!([1, 2]),
            "candidate artifact targets changed",
        ),
        (
            |v| v["image"] = json!({"repository": "ghcr.io/faberline/keep"}),
            "candidate image shape changed",
        ),
        (|v| v["version"] = json!(1), "candidate version is invalid"),
        (|v| v["commit"] = json!(null), "candidate commit is invalid"),
        (
            |v| v["run_attempt"] = "x".into(),
            "candidate run attempt is invalid",
        ),
        (
            |v| v["image"]["arm64_digest"] = "sha256:x".into(),
            "candidate arm64 digest is not an immutable SHA-256 digest",
        ),
        (
            |v| {
                let root = v["image"]["root_digest"].clone();
                v["image"]["arm64_digest"] = root;
            },
            "candidate root and child digests must be distinct",
        ),
    ];
    for (mutate, message) in cases {
        assert_eq!(refused(mutate), format!("GKE receipt refused: {message}"));
    }
}

#[test]
fn schemas_and_workflow_identity_come_from_the_app() {
    use keep_release_gate::candidate::{
        app_targets, candidate_schema, final_jobs, receipt_schema, workflow_ref,
    };
    assert_eq!(candidate_schema("keep"), "cclab.keep.candidate-manifest.v1");
    assert_eq!(receipt_schema("keep"), "keep.gke-release-receipt/v1");
    assert_eq!(
        workflow_ref("keep"),
        "faberline/keep/.github/workflows/keep-release-candidate.yml@refs/heads/main"
    );
    assert_eq!(
        app_targets("keep").unwrap(),
        ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"]
    );
    assert_eq!(final_jobs("keep").len(), 7);
}
