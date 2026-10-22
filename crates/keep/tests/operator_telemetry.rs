//! Operational telemetry wiring in the Kubernetes manifests (#3, TD R5).
//!
//! Every keep pod — whether the operator renders it or it comes from the
//! `k8s/base` direct install — must start with JSONL stdout
//! (`KEEP_LOG_FORMAT=json`, the `axiom.service.log.v1` collector contract) and
//! an explicit request-body bound (`KEEP_BODY_LIMIT`). Collector routing stays
//! deployment-owned, so neither path may carry a Sift setting.
//!
//! Runs in a plain `cargo test -p keep` (keep-operator is a dev-dependency).

use keep_operator::render::render;
use keep_operator::Keep;
use serde_json::{json, Value};

const TELEMETRY_ENV: [&str; 2] = ["KEEP_LOG_FORMAT", "KEEP_BODY_LIMIT"];
const BASE_CONFIGMAP: &str = include_str!("../../../k8s/base/configmap.yaml");
const BASE_STATEFULSET: &str = include_str!("../../../k8s/base/statefulset.yaml");

/// A `Keep` CR as a user would apply it: `spec` merged over the image.
fn keep(spec: Value) -> Keep {
    let mut spec_obj = json!({ "image": "keep:test" });
    spec_obj
        .as_object_mut()
        .unwrap()
        .extend(spec.as_object().unwrap().clone());
    serde_json::from_value(json!({
        "apiVersion": "keep.dev/v1alpha1",
        "kind": "Keep",
        "metadata": { "name": "store" },
        "spec": spec_obj,
    }))
    .expect("Keep CR deserializes")
}

fn of_kind<'a>(objects: &'a [Value], kind: &str) -> &'a Value {
    objects
        .iter()
        .find(|o| o["kind"] == kind)
        .unwrap_or_else(|| panic!("render output has no {kind}"))
}

/// The `configMapKeyRef` (config map name, key) an env var is projected from.
fn config_ref<'a>(container: &'a Value, var: &str) -> Option<(&'a str, &'a str)> {
    container["env"].as_array()?.iter().find_map(|e| {
        if e["name"] != var {
            return None;
        }
        let r = &e["valueFrom"]["configMapKeyRef"];
        Some((r["name"].as_str()?, r["key"].as_str()?))
    })
}

fn assert_no_sift(text: &str, what: &str) {
    assert!(
        !text.to_ascii_lowercase().contains("sift"),
        "{what} must not carry Sift collector configuration"
    );
}

#[test]
fn operator_renders_jsonl_logging_and_body_limit() {
    let objects = render(&keep(json!({ "bodyLimitBytes": 1_048_576 })));

    let cm = of_kind(&objects, "ConfigMap");
    assert_eq!(cm["metadata"]["name"], "store-config");
    assert_eq!(cm["data"]["KEEP_LOG_FORMAT"], "json");
    assert_eq!(cm["data"]["KEEP_BODY_LIMIT"], "1048576");

    let sts = of_kind(&objects, "StatefulSet");
    let container = &sts["spec"]["template"]["spec"]["containers"][0];
    for var in TELEMETRY_ENV {
        assert_eq!(
            config_ref(container, var),
            Some(("store-config", var)),
            "StatefulSet does not project {var} from store-config"
        );
    }
    assert_no_sift(&serde_json::to_string(&objects).unwrap(), "operator render");
}

#[test]
fn base_manifests_match_operator_telemetry_env() {
    let cm: Value = serde_yaml::from_str(BASE_CONFIGMAP).expect("k8s/base/configmap.yaml");
    let sts: Value = serde_yaml::from_str(BASE_STATEFULSET).expect("k8s/base/statefulset.yaml");
    let config = cm["metadata"]["name"].as_str().expect("configmap name");

    assert_eq!(cm["data"]["KEEP_LOG_FORMAT"], "json");
    assert_eq!(
        cm["data"]["KEEP_BODY_LIMIT"],
        keep::http::DEFAULT_BODY_LIMIT.to_string(),
        "k8s/base body limit drifted from the server default"
    );
    // The direct install and an operator-managed pod with default spec agree.
    let operator_objects = render(&keep(json!({})));
    let operator_cm = of_kind(&operator_objects, "ConfigMap");
    for var in TELEMETRY_ENV {
        assert_eq!(
            cm["data"][var], operator_cm["data"][var],
            "k8s/base and operator default disagree on {var}"
        );
    }

    let container = &sts["spec"]["template"]["spec"]["containers"][0];
    for var in TELEMETRY_ENV {
        assert_eq!(
            config_ref(container, var),
            Some((config, var)),
            "k8s/base StatefulSet does not project {var} from {config}"
        );
    }
    assert_no_sift(BASE_CONFIGMAP, "k8s/base/configmap.yaml");
    assert_no_sift(BASE_STATEFULSET, "k8s/base/statefulset.yaml");
}
