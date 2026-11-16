//! Tests for the candidate-workflow target checks. Each refusal asserts the exact text.

use keep_release_gate::candidate_contract::{
    check_candidate, check_candidate_targets, drop_matrix_row, referenced_targets,
};
use keep_release_gate::contract::{find_app, App};
use keep_release_gate::refusal::Refusal;
use std::collections::BTreeSet;

const TWO: [&str; 2] = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];

fn workflow() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows/keep-release-candidate.yml"
    ))
    .unwrap()
}

fn keep() -> App {
    find_app("keep").unwrap()
}

/// The refusal text (the inner message, without the prefix).
fn refused<T: std::fmt::Debug>(result: Result<T, Refusal>) -> String {
    result.unwrap_err().0
}

#[test]
fn passing_real_workflow() {
    assert!(check_candidate_targets(&keep(), &workflow()).is_ok());
    assert!(check_candidate(&keep(), &workflow()).is_ok());
}

#[test]
fn referenced_targets_real_workflow() {
    let manifest = keep_release_gate::workflow_text::job_block(&workflow(), "manifest").unwrap();
    let want: BTreeSet<String> = TWO.iter().map(|s| s.to_string()).collect();
    assert_eq!(referenced_targets(&keep(), &manifest), want);
}

#[test]
fn drop_matrix_row_refused() {
    let text = drop_matrix_row(&keep(), &workflow(), "keep candidate").unwrap();
    assert_eq!(
        refused(check_candidate_targets(&keep(), &text)),
        "keep candidate: build matrix targets ['aarch64-unknown-linux-musl'] differ from apps.sh ['aarch64-unknown-linux-musl', 'x86_64-unknown-linux-musl']"
    );
}

#[test]
fn loop_target_changed_refused() {
    let original = workflow();
    let text = original.replacen(
        "for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do",
        "for target in x86_64-unknown-linux-musl; do",
        1,
    );
    assert_ne!(text, original);
    assert_eq!(
        refused(check_candidate_targets(&keep(), &text)),
        "keep candidate: loop targets ['x86_64-unknown-linux-musl'] differ from apps.sh ['aarch64-unknown-linux-musl', 'x86_64-unknown-linux-musl']"
    );
}

#[test]
fn second_trigger_refused() {
    let original = workflow();
    let text = original.replacen(
        "\non:\n  workflow_dispatch:",
        "\non:\n  push:\n  workflow_dispatch:",
        1,
    );
    assert_ne!(text, original);
    assert_eq!(
        refused(check_candidate(&keep(), &text)),
        "keep candidate: the only trigger must be workflow_dispatch, got ['push', 'workflow_dispatch']"
    );
}

#[test]
fn required_fragment_removed_refused() {
    let original = workflow();
    let text = original.replace("refs/tags/keep@", "refs/tags/keep-");
    assert_ne!(text, original);
    assert_eq!(
        refused(check_candidate(&keep(), &text)),
        "keep candidate: required fragment absent: 'refs/tags/keep@'"
    );
}

#[test]
fn unpinned_action_refused() {
    let original = workflow();
    let text = original.replacen(
        "uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
        "uses: actions/checkout@v4",
        1,
    );
    assert_ne!(text, original);
    assert_eq!(
        refused(check_candidate(&keep(), &text)),
        "keep candidate: action not pinned to a 40-hex commit: actions/checkout@v4"
    );
}

#[test]
fn triple_shape_filters_names() {
    let text = "keep-candidate-x86_64-unknown-linux-musl-${{ github.run_id }} keep-candidate-sboms-${{ github.run_id }}";
    let want: BTreeSet<String> = ["x86_64-unknown-linux-musl".to_string()].into();
    assert_eq!(referenced_targets(&keep(), text), want);
}
