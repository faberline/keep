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

#[test]
fn ha_overlay_sets_the_peer_port_and_mtls_env() {
    let overlay = read("k8s/overlays/ha/kustomization.yaml");
    let expected = [
        ("KEEP_PEER_PORT", "value: \"7118\""),
        ("KEEP_PEER_MTLS", "value: \"on\""),
        ("KEEP_PEER_TLS_CERT", "value: /etc/keep/peer-tls/tls.crt"),
        ("KEEP_PEER_TLS_KEY", "value: /etc/keep/peer-tls/tls.key"),
        ("KEEP_PEER_TLS_CA", "value: /etc/keep/peer-tls/ca.crt"),
    ];
    for (name, value) in expected {
        assert_eq!(
            value_after_env_name(&overlay, name).as_deref(),
            Some(value),
            "the value after `name: {name}` is not `{value}`"
        );
    }
}

#[test]
fn ha_overlay_mounts_the_peer_tls_secret() {
    let overlay = read("k8s/overlays/ha/kustomization.yaml");
    for text in [
        "secretName: keep-peer-tls",
        "mountPath: /etc/keep/peer-tls",
        "readOnly: true",
    ] {
        assert!(
            overlay.contains(text),
            "ha overlay does not contain `{text}`"
        );
    }
}

#[test]
fn ha_overlay_adds_the_peer_port_to_the_headless_service_only() {
    let overlay = read("k8s/overlays/ha/kustomization.yaml");
    assert_eq!(
        overlay.matches("/spec/ports/-").count(),
        1,
        "ha overlay must add one port with `path: /spec/ports/-`"
    );
    assert!(
        overlay.contains("name: keep-headless"),
        "ha overlay has no patch that targets keep-headless"
    );
    assert!(
        overlay.contains("port: 7118"),
        "ha overlay does not add port 7118"
    );
}

#[test]
fn ha_overlay_network_policy_limits_the_peer_port() {
    let overlay = read("k8s/overlays/ha/kustomization.yaml");
    assert!(
        overlay.contains("networkpolicy.yaml"),
        "ha overlay does not list networkpolicy.yaml under resources"
    );
    let policy = read("k8s/overlays/ha/networkpolicy.yaml");
    for text in [
        "kind: NetworkPolicy",
        "Ingress",
        "namespace: keep",
        "app: keep",
        "role: server",
    ] {
        assert!(
            policy.contains(text),
            "networkpolicy.yaml does not contain `{text}`"
        );
    }
    assert_eq!(
        policy.matches("port: 7118").count(),
        1,
        "networkpolicy.yaml must list port 7118 once"
    );
    assert_eq!(
        policy.matches("port: 7117").count(),
        1,
        "networkpolicy.yaml must list port 7117 once"
    );
    let (before_first_from, after_first_from) = policy
        .split_once("from:")
        .expect("networkpolicy.yaml has no `from:`");
    assert!(
        before_first_from.contains("port: 7117"),
        "the client rule for port 7117 must come before the first `from:` and have no `from:`"
    );
    assert!(
        after_first_from.contains("port: 7118"),
        "the peer rule for port 7118 must come after the first `from:`"
    );
    assert!(
        after_first_from.contains("app: keep"),
        "the peer rule must be limited to keep Pods (`app: keep`)"
    );
}
