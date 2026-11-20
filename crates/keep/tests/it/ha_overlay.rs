//! HA overlay: `k8s/overlays/ha` is the install a user applies for a 3-voter
//! raft group. It sets 3 replicas, the raft env, the peer inputs and a
//! PodDisruptionBudget that allows one pod down. It has no test-cluster
//! settings. These tests read repository files as text; no kubectl is run.

use std::path::Path;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|error| panic!("read {}: {error}", full.display()))
}

/// The value line that follows `- name: <name>` in the overlay.
fn value_after_env_name(text: &str, name: &str) -> Option<String> {
    let mut lines = text.lines();
    let marker = format!("- name: {name}");
    while let Some(line) = lines.next() {
        if line.trim() == marker {
            return lines.next().map(|next| next.trim().to_string());
        }
    }
    None
}

#[test]
fn ha_overlay_has_three_replicas() {
    let overlay = read("k8s/overlays/ha/kustomization.yaml");
    assert!(overlay.contains("count: 3"), "ha overlay has no `count: 3`");
    assert!(
        overlay.contains("value: Parallel"),
        "ha overlay does not set podManagementPolicy to Parallel"
    );
}

#[test]
fn ha_overlay_names_the_three_voters_and_peers() {
    let overlay = read("k8s/overlays/ha/kustomization.yaml");
    for name in [
        "SHARD_COUNT",
        "REPLICAS_PER_SHARD",
        "VOTER_COUNT",
        "KEEP_HEADLESS_SERVICE",
        "KEEP_RAFT_MEMBERS_PER_SHARD",
    ] {
        assert!(
            overlay.contains(name),
            "ha overlay does not set the env {name}"
        );
    }
    assert!(
        overlay.contains("value: keep-headless"),
        "ha overlay does not set the headless service value keep-headless"
    );
    assert_eq!(
        value_after_env_name(&overlay, "VOTER_COUNT").as_deref(),
        Some("value: \"3\""),
        "the value after `name: VOTER_COUNT` is not \"3\""
    );
    assert_eq!(
        value_after_env_name(&overlay, "REPLICAS_PER_SHARD").as_deref(),
        Some("value: \"3\""),
        "the value after `name: REPLICAS_PER_SHARD` is not \"3\""
    );
}

#[test]
fn ha_overlay_has_pdb_that_allows_one_down() {
    let overlay = read("k8s/overlays/ha/kustomization.yaml");
    assert!(
        overlay.contains("kind: PodDisruptionBudget"),
        "ha overlay has no PodDisruptionBudget patch"
    );
    assert!(
        overlay.contains("/spec/maxUnavailable"),
        "ha overlay does not patch /spec/maxUnavailable"
    );
    assert!(
        overlay.contains("value: 1"),
        "ha overlay does not set maxUnavailable to 1"
    );
}

#[test]
fn ha_overlay_has_no_test_cluster_settings() {
    let overlay = read("k8s/overlays/ha/kustomization.yaml");
    for setting in ["REPLACE_ME", "ghcr-pull", "nodeSelector", "tolerations"] {
        assert!(
            !overlay.contains(setting),
            "ha overlay contains the test-cluster setting {setting}"
        );
    }
}

#[test]
fn ha_overlay_peer_address_inputs_exist_in_base() {
    let statefulset = read("k8s/base/statefulset.yaml");
    assert!(
        statefulset.contains("serviceName: keep-headless"),
        "base StatefulSet does not use the service keep-headless"
    );
    assert!(
        statefulset.contains("POD_NAME"),
        "base StatefulSet does not set POD_NAME"
    );
    assert!(
        statefulset.contains("POD_NAMESPACE"),
        "base StatefulSet does not set POD_NAMESPACE"
    );
    let service = read("k8s/base/service.yaml");
    assert!(
        service.contains("name: keep-headless"),
        "base service file has no keep-headless service"
    );
    assert!(
        service.contains("clusterIP: None"),
        "keep-headless is not a headless service"
    );
}

#[test]
fn quickstart_names_the_ha_overlay() {
    let quickstart = read("docs/product/quickstart.md");
    assert!(
        quickstart.contains("kubectl apply -k k8s/overlays/ha"),
        "quickstart does not name `kubectl apply -k k8s/overlays/ha`"
    );
    assert!(
        quickstart.contains("## Run three replicas (HA)"),
        "quickstart has no section `## Run three replicas (HA)`"
    );
}

#[test]
fn ha_overlay_keeps_base_single_replica() {
    let statefulset = read("k8s/base/statefulset.yaml");
    assert!(
        statefulset.contains("replicas: 1"),
        "base StatefulSet no longer has `replicas: 1`"
    );
}
