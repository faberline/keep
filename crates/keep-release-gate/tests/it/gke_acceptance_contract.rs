//! Tests for the gke-acceptance workflow checks. Each refusal asserts the exact text.

use keep_release_gate::contract::{find_app, App, ContractError};
use keep_release_gate::gke_acceptance_contract::{
    build_gate, check_gke_acceptance, check_gke_acceptance_names, env_line,
};
use keep_release_gate::promotion_contract::check_app;
use keep_release_gate::workflow_text::{job_block, step_block};
use std::path::{Path, PathBuf};

const WORKFLOW: &str = ".github/workflows/gke-acceptance.yml";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn workflow() -> String {
    std::fs::read_to_string(root().join(WORKFLOW)).unwrap()
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

/// Recursively copy the directory `from` to `to`.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A temporary root with copies of `.github` and `scripts`, whose gke-acceptance
/// workflow is `gke_text`.
fn temp_root(gke_text: &str) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    copy_dir(&root().join(".github"), &dir.path().join(".github"));
    copy_dir(&root().join("scripts"), &dir.path().join("scripts"));
    std::fs::write(dir.path().join(WORKFLOW), gke_text).unwrap();
    dir
}

#[test]
fn real_workflow_passes() {
    let text = workflow();
    check_gke_acceptance(&text).unwrap();
    check_gke_acceptance_names(&text).unwrap();
}

#[test]
fn check_app_runs_the_real_gke_checks() {
    check_app(&root(), &keep()).unwrap();
}

#[test]
fn check_app_runs_gke_checks_on_the_tree() {
    // The copy passes, so the refusal below comes from the gke checks in check_app.
    let copy = temp_root(&workflow());
    check_app(copy.path(), &keep()).unwrap();

    let renamed = mutate(&workflow(), "name: deploy + verify on GKE", "name: deploy");
    let copy = temp_root(&renamed);
    assert_eq!(
        refused(check_app(copy.path(), &keep())),
        "gke-acceptance: the acceptance job must be named 'deploy + verify on GKE'"
    );
}

#[test]
fn helper_strings() {
    let text = workflow();
    assert_eq!(
        env_line("keep"),
        "KEEP_IMAGE: ${{ contains(inputs.apps, 'keep') && inputs.image || needs.build-keep.outputs.image }}"
    );
    assert_eq!(
        build_gate("keep"),
        "if: contains(inputs.apps, 'keep') && inputs.image == ''"
    );
    assert!(text.contains(&env_line("keep")));
    assert!(text.contains(&build_gate("keep")));
}

#[test]
fn second_trigger() {
    let text = mutate(
        &workflow(),
        "on:\n  workflow_dispatch:",
        "on:\n  push:\n  workflow_dispatch:",
    );
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: the only trigger must be workflow_dispatch, got ['push', 'workflow_dispatch']"
    );
}

#[test]
fn inputs_changed() {
    let text = mutate(&workflow(), "      ref:\n", "      rev:\n");
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: inputs must be apps, ref, image, topology, got ['apps', 'image', 'rev', 'topology']"
    );
}

#[test]
fn extra_input_is_refused() {
    let text = mutate(
        &workflow(),
        "      image:\n",
        "      extra:\n      image:\n",
    );
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: inputs must be apps, ref, image, topology, got ['apps', 'extra', 'image', 'ref', 'topology']"
    );
}

#[test]
fn topology_input_is_allowed() {
    let text = workflow();
    assert!(text.contains("      topology:\n"));
    check_gke_acceptance(&text).unwrap();
}

#[test]
fn missing_topology_input_is_refused() {
    let text = mutate(&workflow(), "      topology:\n", "      topo:\n");
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: inputs must be apps, ref, image, topology, got ['apps', 'image', 'ref', 'topo']"
    );
}

#[test]
fn run_name_without_image() {
    let text = mutate(&workflow(), "image={0}", "img={0}");
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: run-name must carry the prebuilt image so release.sh can tell the runs apart"
    );
}

#[test]
fn job_renamed() {
    let text = mutate(&workflow(), "  build-keep:", "  build-x:");
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: jobs must be ['acceptance', 'build-keep'], got ['acceptance', 'build-x']"
    );
}

#[test]
fn build_gate_removed() {
    let text = mutate(
        &workflow(),
        "if: contains(inputs.apps, 'keep') && inputs.image == ''",
        "if: contains(inputs.apps, 'keep')",
    );
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: build-keep must be skipped when a prebuilt image is given"
    );
}

#[test]
fn acceptance_job_renamed() {
    let text = mutate(&workflow(), "name: deploy + verify on GKE", "name: deploy");
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: the acceptance job must be named 'deploy + verify on GKE'"
    );
}

#[test]
fn names_job_renamed() {
    let text = mutate(&workflow(), "name: deploy + verify on GKE", "name: deploy");
    assert_eq!(
        refused(check_gke_acceptance_names(&text)),
        "gke-acceptance: the acceptance job must be named 'deploy + verify on GKE' for make-gke-release-receipt.py"
    );
}

#[test]
fn image_env_line_removed() {
    let text = mutate(&workflow(), &env_line("keep"), "KEEP_IMAGE: x");
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: KEEP_IMAGE must prefer the prebuilt image for a selected app"
    );
}

#[test]
fn verify_step_condition_removed() {
    let text = mutate(&workflow(), "if: inputs.image != ''", "if: always()");
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: 'Verify prebuilt image input' must run whenever a prebuilt image is given"
    );
}

#[test]
fn verify_step_after_prepare() {
    let text = workflow();
    let acceptance = job_block(&text, "acceptance").unwrap();
    let verify = step_block(&acceptance, "Verify prebuilt image input").unwrap();
    // Move the verify step to just before the harness step, which comes after the prepare step.
    let moved = mutate(&text, &verify, "");
    let moved = moved.replacen(
        "      - name: Run acceptance harness",
        &format!("{verify}\n      - name: Run acceptance harness"),
        1,
    );
    assert_eq!(
        refused(check_gke_acceptance(&moved)),
        "gke-acceptance: 'Verify prebuilt image input' must run before any cluster work"
    );
}

#[test]
fn one_app_check_removed() {
    let text = mutate(&workflow(), "[ \"${#apps[@]}\" -eq 1 ]", "true");
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: 'Verify prebuilt image input' must check '[ \"${#apps[@]}\" -eq 1 ]'"
    );
}

#[test]
fn owner_line_removed() {
    let text = mutate(
        &workflow(),
        "OWNER: ${{ github.repository_owner }}",
        "OWNER: x",
    );
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "gke-acceptance: the image owner must come from github.repository_owner"
    );
}

#[test]
fn harness_step_missing() {
    let text = mutate(
        &workflow(),
        "- name: Run acceptance harness",
        "- name: Run harness",
    );
    assert_eq!(
        refused(check_gke_acceptance(&text)),
        "step 'Run acceptance harness' must appear exactly once, found 0"
    );
}

#[test]
fn verdict_line_removed() {
    let text = mutate(
        &workflow(),
        "[keep] PASS: readyz + round-trip + durability",
        "[keep] PASS",
    );
    check_gke_acceptance(&text).unwrap();
    assert_eq!(
        refused(check_gke_acceptance_names(&text)),
        "gke-acceptance: the verdict line must be '[keep] PASS: readyz + round-trip + durability'"
    );
}
