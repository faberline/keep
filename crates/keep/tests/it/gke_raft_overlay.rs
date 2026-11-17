//! GKE raft overlay: the 3-replica raft topology of the shared test cluster
//! renders from its own overlay and Job, the single-replica overlay stays as
//! it was, and the acceptance workflow runs only by hand. These tests read
//! repository files as text; no kubectl, shellcheck or workflow is run.

use std::path::Path;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|error| panic!("read {}: {error}", full.display()))
}

#[test]
fn raft_overlay_has_three_replicas_and_raft_env() {
    let overlay = read("k8s/overlays/gke-test-raft/kustomization.yaml");
    assert!(
        overlay.contains("count: 3"),
        "raft overlay has no `count: 3`"
    );
    assert!(
        overlay.contains("value: Parallel"),
        "raft overlay does not set podManagementPolicy to Parallel"
    );
    for name in [
        "SHARD_COUNT",
        "REPLICAS_PER_SHARD",
        "VOTER_COUNT",
        "KEEP_HEADLESS_SERVICE",
        "KEEP_RAFT_MEMBERS_PER_SHARD",
    ] {
        assert!(
            overlay.contains(name),
            "raft overlay does not set the env {name}"
        );
    }
}

#[test]
fn raft_overlay_keeps_single_overlay_untouched() {
    let overlay = read("k8s/overlays/gke-test/kustomization.yaml");
    assert!(
        overlay.contains("count: 1"),
        "the single overlay no longer has `count: 1`"
    );
}

#[test]
fn raft_verify_script_has_all_steps() {
    let script = read("k8s/gke-test-job-raft/verify-raft.sh");
    for step in 1..=6 {
        let marker = format!("[keep] step {step}:");
        assert!(script.contains(&marker), "verify-raft.sh lacks {marker}");
    }
    assert!(
        script.contains("[keep] PASS: raft 3 replicas + leader failover + durability"),
        "verify-raft.sh lacks the pass line"
    );
    assert!(
        script.contains("is_leader"),
        "verify-raft.sh does not read is_leader"
    );
}

#[test]
fn acceptance_workflow_has_topology_input_and_no_new_trigger() {
    let workflow = read(".github/workflows/gke-acceptance.yml");
    assert!(
        workflow.contains("topology:"),
        "workflow has no topology input"
    );
    assert!(
        workflow.contains("k8s/overlays/gke-test-raft"),
        "workflow does not select the raft overlay"
    );
    assert!(
        workflow.contains("workflow_dispatch:"),
        "workflow lost its workflow_dispatch trigger"
    );
    // Only the trigger block (before `permissions:`) decides when the run
    // starts. Comment lines may name the triggers, so check trigger keys only.
    let head = workflow
        .split("\npermissions:")
        .next()
        .expect("workflow has no permissions block");
    for trigger in ["pull_request", "push", "schedule"] {
        let key = format!("{trigger}:");
        let found = head
            .lines()
            .any(|line| !line.trim_start().starts_with('#') && line.trim_start().starts_with(&key));
        assert!(!found, "workflow has a new trigger `{trigger}`");
    }
}

#[test]
fn ci_renders_raft_overlay() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        ci.contains("kubectl kustomize k8s/overlays/gke-test-raft"),
        "ci.yml does not render the raft overlay"
    );
    assert!(
        ci.contains("shellcheck -x -S warning k8s/gke-test-job-raft/verify-raft.sh"),
        "ci.yml does not shellcheck verify-raft.sh"
    );
}
