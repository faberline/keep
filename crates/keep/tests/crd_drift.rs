//! CRD drift gate (#5): the committed `k8s/operator/crd.yaml` must be exactly
//! what the generator emits from `KeepSpec`. A field added to the Rust spec but
//! missing from the applied CRD is pruned by the API server without any error,
//! so the operator silently behaves as if the user never set it.
//!
//! Runs in a plain `cargo test -p keep` (the generator is a dev-dependency), so
//! it does not depend on CI remembering `--features operator`.

const REGENERATE: &str =
    "cargo run -p keep --features operator --bin keep -- k8s crd render --out k8s/operator/crd.yaml";

#[test]
fn committed_crd_matches_generator() {
    let committed = include_str!("../../../k8s/operator/crd.yaml");
    let generated = keep_operator::crd_yaml();
    if committed == generated {
        return;
    }
    let line = committed
        .lines()
        .zip(generated.lines())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| committed.lines().count().min(generated.lines().count()))
        + 1;
    panic!(
        "k8s/operator/crd.yaml is stale (first difference at line {line}); regenerate it:\n  {REGENERATE}"
    );
}
