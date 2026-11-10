//! Operator-adoption integration tests (#775). Compiled only with
//! `--features operator`.
//!
//! - R1: `KeepSpec` flattens `service_k8s::ClusterSpec` into the CRD schema.
//! - R2: `render` emits the sharded StatefulSet with the downward-API env
//!   raft-runtime consumes, correct replica math, and status phases.
//! - R4: the generated CRD is Kubernetes-OpenAPI compatible (no `uint32`/
//!   `uint64`; unsigned counts keep `minimum`).
//! - R5: the process-level rustls crypto provider install is idempotent.
//!
//! @spec .aw/tech-design/projects/keep/interfaces/cli/adopt-libs-operator-keep-k8s-crd-operator-instance-cli.md
#![cfg(feature = "operator")]

use std::collections::HashMap;

use keep::operator::render::render;
use keep::operator::{crd_yaml, Keep, KeepSpec};
use keep::tls::install_default_crypto_provider;
use serde_json::Value;
use service_k8s::{ClusterSpec, ManagedService, ReadyFacts};

fn spec(shard_count: u32, replicas: u32) -> KeepSpec {
    KeepSpec {
        cluster: ClusterSpec {
            image: "keep:test".into(),
            image_pull_policy: None,
            shard_count,
            replicas_per_shard: replicas,
            voter_count: replicas,
            resources: Default::default(),
        },
        engine_shards: 256,
        log_level: None,
        grace_secs: 30,
        storage: "10Gi".into(),
        storage_class: None,
        body_limit_bytes: 16 * 1024 * 1024,
        maxmemory: None,
        eviction_policy: None,
        spill_threshold_bytes: None,
        max_disk_bytes: None,
        backup: None,
    }
}

fn of_kind<'a>(objs: &'a [Value], kind: &str) -> &'a Value {
    objs.iter()
        .find(|o| o["kind"] == kind)
        .unwrap_or_else(|| panic!("render output has no {kind}"))
}

/// R1 — the flattened `ClusterSpec` fields sit directly under the CRD's `spec`
/// schema (no `cluster` wrapper), so a `Keep` CR declares them inline.
#[test]
fn crd_flattens_cluster_spec() {
    let yaml = crd_yaml();
    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("CRD parses as YAML");
    let props = &doc["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
        ["properties"];
    for field in [
        "image",
        "imagePullPolicy",
        "shardCount",
        "replicasPerShard",
        "voterCount",
        "resources",
    ] {
        assert!(
            props.get(field).is_some(),
            "CRD spec schema must carry flattened field `{field}`; props = {props:?}"
        );
    }
    // Flatten merges properties → there is no nested `cluster` wrapper property.
    assert!(
        props.get("cluster").is_none(),
        "ClusterSpec must be flattened, not nested under `cluster`"
    );
    // keep's own knobs are present too.
    assert!(props.get("engineShards").is_some());
    assert!(props.get("storage").is_some());
}

/// R2 — the rendered StatefulSet carries the downward-API env raft-runtime reads,
/// the right replica count, keep's runtime knobs + disk tier, and drives a
/// readiness/status contract.
#[test]
fn render_emits_downward_api_statefulset() {
    let keep = Keep::new("store", spec(2, 3));
    let objs = render(&keep);

    let sts = of_kind(&objs, "StatefulSet");
    assert_eq!(
        sts["spec"]["replicas"], 6,
        "replicas = shardCount * replicasPerShard"
    );
    assert_eq!(sts["spec"]["serviceName"], "store-headless");
    assert_eq!(sts["spec"]["podManagementPolicy"], "Parallel");

    let env = sts["spec"]["template"]["spec"]["containers"][0]["env"]
        .as_array()
        .expect("container env array");
    let keys: Vec<&str> = env.iter().map(|e| e["name"].as_str().unwrap()).collect();
    for k in [
        "POD_NAME",
        "POD_NAMESPACE",
        "SHARD_COUNT",
        "REPLICAS_PER_SHARD",
        "VOTER_COUNT",
        "KEEP_HEADLESS_SERVICE",
    ] {
        assert!(keys.contains(&k), "missing downward-API env {k}");
    }
    for k in ["KEEP_HOST", "KEEP_DATA_DIR", "KEEP_SHARD_COUNT"] {
        assert!(keys.contains(&k), "missing keep runtime env {k}");
    }
    // Durable disk tier + production hardening survive the enrichment.
    assert!(sts["spec"]["volumeClaimTemplates"].is_array());
    assert_eq!(
        sts["spec"]["template"]["spec"]["containers"][0]["readinessProbe"]["httpGet"]["path"],
        "/readyz"
    );
    assert_eq!(
        sts["spec"]["template"]["spec"]["containers"][0]["securityContext"]
            ["readOnlyRootFilesystem"],
        true
    );
    let container = &sts["spec"]["template"]["spec"]["containers"][0];
    assert_eq!(container["resources"]["requests"]["cpu"], "1");
    assert_eq!(container["resources"]["requests"]["memory"], "4Gi");
    assert!(container["resources"].get("limits").is_none());

    // The rest of the child set is present.
    assert_eq!(
        of_kind(&objs, "ServiceAccount")["metadata"]["name"],
        "store"
    );
    assert_eq!(
        of_kind(&objs, "ConfigMap")["metadata"]["name"],
        "store-config"
    );
    assert_eq!(
        of_kind(&objs, "PodDisruptionBudget")["spec"]["maxUnavailable"],
        0
    );

    // Readiness target + status phases.
    let targets = keep.readiness_targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].kind, "StatefulSet");

    let mut all_ready = HashMap::new();
    all_ready.insert("store".to_string(), 6i64);
    let status = keep.status_patch(&ReadyFacts { ready: all_ready });
    assert_eq!(status["status"]["phase"], "Ready");
    assert_eq!(status["status"]["desiredReplicas"], 6);
    assert_eq!(status["status"]["shardCount"], 2);

    let mut partial = HashMap::new();
    partial.insert("store".to_string(), 2i64);
    let status = keep.status_patch(&ReadyFacts { ready: partial });
    assert_eq!(status["status"]["phase"], "Reconciling");

    let status = keep.status_patch(&ReadyFacts {
        ready: HashMap::new(),
    });
    assert_eq!(status["status"]["phase"], "Pending");
}

/// R3 (#776) — the operator renders a backup CronJob invoking `keep backup`
/// when `spec.backup` is configured, and omits it otherwise.
#[test]
fn render_emits_backup_cronjob_when_configured() {
    use keep::operator::KeepBackupSpec;

    // No policy -> no CronJob.
    let plain = Keep::new("store", spec(1, 1));
    assert!(
        render(&plain).iter().all(|o| o["kind"] != "CronJob"),
        "no backup policy must render no CronJob"
    );

    // Policy set -> a CronJob invoking `keep backup` with the schedule + dest +
    // retention flags.
    let mut s = spec(1, 1);
    s.backup = Some(KeepBackupSpec {
        schedule: "17 3 * * *".into(),
        destination: "s3://bucket/keep".into(),
        retention_secs: Some(604_800),
    });
    let keep = Keep::new("store", s);
    let objs = render(&keep);

    let cj = of_kind(&objs, "CronJob");
    assert_eq!(cj["metadata"]["name"], "store-backup");
    assert_eq!(cj["spec"]["schedule"], "17 3 * * *");
    let container = &cj["spec"]["jobTemplate"]["spec"]["template"]["spec"]["containers"][0];
    assert_eq!(container["command"][0], "keep");
    let args: Vec<&str> = container["args"]
        .as_array()
        .expect("args array")
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(args[0], "backup");
    assert!(args.contains(&"--dest"));
    assert!(args.contains(&"s3://bucket/keep"));
    assert!(args.contains(&"--retention-secs"));
    assert!(args.contains(&"604800"));
    // The runner runs under keep's ServiceAccount.
    assert_eq!(
        cj["spec"]["jobTemplate"]["spec"]["template"]["spec"]["serviceAccountName"],
        "store"
    );
}

/// R4 — the generated CRD is Kubernetes-OpenAPI compatible.
#[test]
fn crd_is_openapi_compatible() {
    let yaml = crd_yaml();
    assert!(
        !yaml.contains("uint32"),
        "CRD must not carry format: uint32"
    );
    assert!(
        !yaml.contains("uint64"),
        "CRD must not carry format: uint64"
    );
    // The unsigned counts were normalized to integer with a minimum floor.
    assert!(
        yaml.contains("minimum"),
        "normalized uints keep a minimum floor"
    );

    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("CRD parses as YAML");
    let backup_props = &doc["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]
        ["spec"]["properties"]["backup"]["properties"];
    for field in ["schedule", "destination", "retentionSecs"] {
        assert!(
            backup_props.get(field).is_some(),
            "shared backup schema must keep flat field `{field}`"
        );
    }
    assert_eq!(backup_props["destination"]["type"], "string");
}

/// R5 — installing the process-level rustls crypto provider is idempotent and
/// safe to call repeatedly (as `main` does before command parsing).
#[test]
fn rustls_provider_install_is_idempotent() {
    install_default_crypto_provider();
    install_default_crypto_provider();
}

/// The server container's env, cloned. Panics when the StatefulSet has no env array.
fn server_env(objs: &[Value]) -> Vec<Value> {
    of_kind(objs, "StatefulSet")["spec"]["template"]["spec"]["containers"][0]["env"]
        .as_array()
        .expect("server container env array")
        .clone()
}

/// The four operator-written ConfigMap keys that the server container must read.
const CONFIG_KEYS: [&str; 4] = [
    "KEEP_MAXMEMORY",
    "KEEP_EVICTION_POLICY",
    "KEEP_SPILL_THRESHOLD_BYTES",
    "KEEP_MAX_DISK_BYTES",
];

/// A spec with all four ConfigMap-backed knobs set.
fn full_spec() -> KeepSpec {
    let mut s = spec(1, 1);
    s.maxmemory = Some("512Mi".into());
    s.eviction_policy = Some("noeviction".into());
    s.spill_threshold_bytes = Some(1024);
    s.max_disk_bytes = Some("10Gi".into());
    s
}

/// Operator-written ConfigMap knobs reach the server container as exactly one
/// `configMapKeyRef` env entry each when the spec sets them.
#[test]
fn configmap_keys_reach_the_server_container_when_set() {
    let objs = render(&Keep::new("store", full_spec()));
    let env = server_env(&objs);
    for key in CONFIG_KEYS {
        let matches: Vec<&Value> = env.iter().filter(|e| e["name"] == key).collect();
        assert_eq!(
            matches.len(),
            1,
            "server container must have exactly one {key} env entry"
        );
        assert_eq!(
            matches[0]["valueFrom"]["configMapKeyRef"]["name"],
            "store-config"
        );
        assert_eq!(matches[0]["valueFrom"]["configMapKeyRef"]["key"], key);
    }
    let data = &of_kind(&objs, "ConfigMap")["data"];
    assert_eq!(data["KEEP_MAXMEMORY"], "512Mi");
    assert_eq!(data["KEEP_SPILL_THRESHOLD_BYTES"], "1024");
}

/// Every ConfigMap key is consumed by the server container, and no env name
/// appears twice, both with the knobs unset and with them set.
#[test]
fn every_configmap_key_is_consumed_and_env_names_are_unique() {
    for (label, s) in [("unset", spec(1, 1)), ("set", full_spec())] {
        let objs = render(&Keep::new("store", s));
        let env = server_env(&objs);

        let consumed: Vec<&str> = env
            .iter()
            .filter_map(|e| e["valueFrom"]["configMapKeyRef"]["key"].as_str())
            .collect();
        let data = of_kind(&objs, "ConfigMap")["data"]
            .as_object()
            .expect("ConfigMap data object");
        for k in data.keys() {
            assert!(
                consumed.contains(&k.as_str()),
                "ConfigMap key {k} is not consumed by the server container ({label})"
            );
        }

        let mut names: Vec<&str> = env
            .iter()
            .map(|e| e["name"].as_str().expect("env name string"))
            .collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "server container env names must be unique ({label})"
        );

        if label == "unset" {
            for key in CONFIG_KEYS {
                assert!(
                    !names.contains(&key),
                    "unset spec must not render {key} into the server container"
                );
            }
        }
    }
}
