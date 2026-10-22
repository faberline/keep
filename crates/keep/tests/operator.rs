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
    let pod = &cj["spec"]["jobTemplate"]["spec"]["template"]["spec"];
    assert_eq!(pod["serviceAccountName"], "store");

    // It reads pod 0's data PVC read-only at the `--data-dir` it passes...
    assert_eq!(pod["volumes"][0]["name"], "data");
    assert_eq!(
        pod["volumes"][0]["persistentVolumeClaim"],
        serde_json::json!({ "claimName": "data-store-0", "readOnly": true })
    );
    assert_eq!(
        container["volumeMounts"][0],
        serde_json::json!({ "name": "data", "mountPath": "/data", "readOnly": true })
    );
    let data_dir = args.iter().position(|a| *a == "--data-dir").unwrap() + 1;
    assert_eq!(args[data_dir], "/data");
    // ...on pod 0's node (the claim is ReadWriteOnce), as keep's user.
    let term = &pod["affinity"]["podAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"][0];
    assert_eq!(
        term["labelSelector"]["matchLabels"]["statefulset.kubernetes.io/pod-name"],
        "store-0"
    );
    assert_eq!(term["topologyKey"], "kubernetes.io/hostname");
    assert_eq!(pod["securityContext"]["runAsUser"], 65532);
    assert_eq!(pod["securityContext"]["fsGroup"], 65532);
}

/// R4 — the generated CRD is Kubernetes-OpenAPI compatible.
/// The operator applies every rendered child with server-side apply, so the
/// ClusterRole must grant `patch` on each child kind — a missing rule only shows
/// up as a 403 reconcile loop on a real cluster (the backup CronJob shipped
/// that way).
#[test]
fn rbac_covers_every_rendered_kind() {
    use keep::operator::KeepBackupSpec;

    let rbac: Vec<serde_yaml::Value> =
        serde_yaml::Deserializer::from_str(include_str!("../../../k8s/operator/rbac.yaml"))
            .map(|d| serde::Deserialize::deserialize(d).expect("rbac.yaml document"))
            .collect();
    let role = rbac
        .iter()
        .find(|d| d["kind"] == "ClusterRole")
        .expect("ClusterRole in rbac.yaml");
    let strs = |v: &serde_yaml::Value| -> Vec<String> {
        v.as_sequence()
            .map(|s| {
                s.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let grants = |group: &str, resource: &str| {
        role["rules"].as_sequence().into_iter().flatten().any(|r| {
            strs(&r["apiGroups"]).iter().any(|g| g == group)
                && strs(&r["resources"]).iter().any(|x| x == resource)
                && strs(&r["verbs"]).iter().any(|v| v == "patch")
        })
    };

    let mut s = spec(2, 3);
    s.backup = Some(KeepBackupSpec {
        schedule: "0 3 * * *".into(),
        destination: "file:///tmp/keep-backups".into(),
        retention_secs: None,
    });
    for obj in render(&Keep::new("store", s)) {
        let api_version = obj["apiVersion"].as_str().expect("apiVersion");
        let group = api_version.rsplit_once('/').map_or("", |(g, _)| g);
        let kind = obj["kind"].as_str().expect("kind");
        let resource = format!("{}s", kind.to_ascii_lowercase());
        assert!(
            grants(group, &resource),
            "k8s/operator/rbac.yaml grants no `patch` on {group}/{resource} (rendered {kind})"
        );
    }
}

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
