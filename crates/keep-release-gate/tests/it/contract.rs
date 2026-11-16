//! Tests for the App table and the contract error of verify-release-contract.

use keep_release_gate::contract::{apps, fail, find_app, shared_jobs};
use std::path::PathBuf;

#[test]
fn error_text_has_no_prefix() {
    assert_eq!(fail::<()>("boom").unwrap_err().to_string(), "boom");
}

#[test]
fn keep_row_matches_the_python_table() {
    let rows = apps();
    assert_eq!(rows.len(), 1);
    let keep = find_app("keep").unwrap();
    assert_eq!(keep, rows[0]);
    assert_eq!(keep.name, "keep");
    assert_eq!(keep.root, ".");
    assert_eq!(keep.scripts_dir, "scripts/release");
    assert!(keep.shared_scripts);
    assert!(keep.onboarded);
    assert_eq!(
        keep.targets,
        vec!["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"]
    );
    assert_eq!(keep.candidate_jobs, shared_jobs("keep"));
    assert_eq!(keep.promotion_prefix, "gke_receipt");
    assert!(keep.takes_attempt);
    assert_eq!(keep.manifest_schema, "cclab.keep.candidate-manifest.v1");
    assert_eq!(keep.receipt_name, "keep-gke-receipt.json");
    assert_eq!(keep.receipt_schema, "keep.gke-release-receipt/v1");
    assert_eq!(keep.gke_backend, "gke-acceptance");
    assert_eq!(keep.functional, vec!["readyz", "round_trip", "durability"]);
}

#[test]
fn shared_jobs_names_the_gates_job() {
    assert_eq!(
        shared_jobs("keep"),
        vec![
            "identity",
            "build",
            "keep-release-gates",
            "ghcr-image-and-attest",
            "manifest",
            "verify-candidate",
            "result",
        ]
    );
}

#[test]
fn promotion_inputs_follow_takes_attempt() {
    let mut keep = find_app("keep").unwrap();
    assert_eq!(
        keep.promotion_inputs(),
        vec![
            "version",
            "candidate_run_id",
            "candidate_run_attempt",
            "gke_receipt_b64",
            "gke_receipt_sha256",
            "gke_receipt_sidecar_b64",
            "gke_receipt_sidecar_sha256",
        ]
    );
    keep.takes_attempt = false;
    assert_eq!(keep.promotion_inputs().len(), 6);
    assert!(!keep
        .promotion_inputs()
        .contains(&"candidate_run_attempt".to_string()));
}

#[test]
fn workflow_paths() {
    let keep = find_app("keep").unwrap();
    assert_eq!(
        keep.candidate_workflow(),
        PathBuf::from(".github/workflows/keep-release-candidate.yml")
    );
    assert_eq!(
        keep.promotion_workflow(),
        PathBuf::from(".github/workflows/keep-release.yml")
    );
}

#[test]
fn unknown_app_is_refused() {
    assert_eq!(
        find_app("lumen").unwrap_err().to_string(),
        "unknown app: lumen (known: keep)"
    );
}
