//! Tests for the promotion-workflow checks. Each refusal asserts the exact text.

use keep_release_gate::contract::{find_app, App, ContractError};
use keep_release_gate::promotion_contract::{
    bash_variable, check_app, check_not_onboarded, check_promotion, is_shared_release_pin,
    read_workflow,
};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn workflow() -> String {
    std::fs::read_to_string(root().join(".github/workflows/keep-release.yml")).unwrap()
}

fn keep() -> App {
    find_app("keep").unwrap()
}

/// The refusal text (the inner message, without any prefix).
fn refused<T: std::fmt::Debug>(result: Result<T, ContractError>) -> String {
    match result {
        Err(error) => error.0,
        Ok(value) => panic!("expected a refusal, got {value:?}"),
    }
}

/// `text` with the first `from` replaced by `to`, after checking that `from` occurs.
fn mutate(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "fixture lacks {from:?}");
    let mutated = text.replacen(from, to, 1);
    assert_ne!(mutated, text);
    mutated
}

#[test]
fn passing_real_workflow() {
    check_promotion(&root(), &keep(), &workflow()).unwrap();
    check_app(&root(), &keep()).unwrap();
}

#[test]
fn unpinned_shared_workflow_refused() {
    let text = mutate(
        &workflow(),
        "service-release.yml@eddb20c39f82655f1b41348230f5bb0925827122",
        "service-release.yml@main",
    );
    assert_eq!(
        refused(check_promotion(&root(), &keep(), &text)),
        "keep promotion: the only uses must be faberline/aw service-release.yml pinned to a 40-hex commit, got ['faberline/aw/.github/workflows/service-release.yml@main']"
    );
}

#[test]
fn missing_permission_refused() {
    let text = mutate(&workflow(), "      packages: write\n", "");
    assert_eq!(
        refused(check_promotion(&root(), &keep(), &text)),
        "keep promotion: release permissions must be exactly ['actions: read', 'attestations: read', 'contents: write', 'packages: write', 'pull-requests: read'], got ['actions: read', 'attestations: read', 'contents: write', 'pull-requests: read']"
    );
}

#[test]
fn changed_input_refused() {
    let text = mutate(
        &workflow(),
        "      candidate_run_attempt:\n",
        "      candidate_run_tries:\n",
    );
    assert_eq!(
        refused(check_promotion(&root(), &keep(), &text)),
        "keep promotion: inputs must be ['candidate_run_attempt', 'candidate_run_id', 'gke_receipt_b64', 'gke_receipt_sha256', 'gke_receipt_sidecar_b64', 'gke_receipt_sidecar_sha256', 'version'], got ['candidate_run_id', 'candidate_run_tries', 'gke_receipt_b64', 'gke_receipt_sha256', 'gke_receipt_sidecar_b64', 'gke_receipt_sidecar_sha256', 'version']"
    );
}

#[test]
fn extra_trigger_refused() {
    let text = mutate(
        &workflow(),
        "on:\n  workflow_dispatch:",
        "on:\n  push:\n  workflow_dispatch:",
    );
    assert_eq!(
        refused(check_promotion(&root(), &keep(), &text)),
        "keep promotion: the only trigger must be workflow_dispatch, got ['push', 'workflow_dispatch']"
    );
}

#[test]
fn compatibility_note_changed_refused() {
    let text = mutate(&workflow(), "no HTTP, CLI", "no HTTP");
    assert_eq!(
        refused(check_promotion(&root(), &keep(), &text)),
        "keep promotion: with compatibility_note must be the apps.sh RELEASE_COMPATIBILITY_LINE as a double-quoted string"
    );
}

#[test]
fn is_shared_release_pin_cases() {
    let prefix = "faberline/aw/.github/workflows/service-release.yml@";
    assert!(is_shared_release_pin(&format!(
        "{prefix}{}",
        "a".repeat(40)
    )));
    assert!(!is_shared_release_pin(&format!(
        "{prefix}{}",
        "a".repeat(39)
    )));
    assert!(!is_shared_release_pin(&format!(
        "{prefix}{}",
        "a".repeat(41)
    )));
    assert!(!is_shared_release_pin(&format!(
        "{prefix}{}",
        "A".repeat(40)
    )));
    assert!(!is_shared_release_pin(&format!(
        "other/aw/.github/workflows/service-release.yml@{}",
        "a".repeat(40)
    )));
}

#[test]
fn bash_variable_reads_apps_sh() {
    assert_eq!(
        bash_variable(&root(), "RELEASE_COMPATIBILITY_LINE").unwrap(),
        "- Compatibility: no HTTP, CLI, wire format, on-disk format, or Kubernetes manifest shape changed in this release."
    );
}

#[test]
fn bash_variable_missing_refused() {
    let text = refused(bash_variable(&root(), "NO_SUCH_VARIABLE_X"));
    assert!(
        text.starts_with("apps.sh does not define NO_SUCH_VARIABLE_X: exit "),
        "got {text}"
    );
}

#[test]
fn read_workflow_missing_refused() {
    assert_eq!(
        refused(read_workflow(
            &root(),
            Path::new(".github/workflows/nothing.yml")
        )),
        "missing file: .github/workflows/nothing.yml"
    );
}

#[test]
fn not_onboarded_app_refused() {
    let mut app = keep();
    app.onboarded = false;
    assert_eq!(
        refused(check_app(&root(), &app)),
        "keep is not onboarded yet: its release workflows do not exist (flip onboarded when they land)"
    );
    assert_eq!(
        refused(check_not_onboarded(&root(), &app)),
        ".github/workflows/keep-release-candidate.yml exists but keep is still marked onboarded=False here"
    );
}
