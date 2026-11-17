//! The keep test image is built with the `raft` cargo feature
//! (`Dockerfile` build-arg `CARGO_FEATURES`, set by `keep-test-image.yml`).
//! The files are read as text. The default Docker build must stay featureless.

const DOCKERFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Dockerfile");
const TEST_IMAGE_WORKFLOW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/keep-test-image.yml"
);

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn dockerfile_has_cargo_features_arg() {
    let text = read(DOCKERFILE);
    assert!(
        text.lines().any(|l| l == r#"ARG CARGO_FEATURES="""#),
        "Dockerfile has no `ARG CARGO_FEATURES=\"\"` line (empty default)"
    );
    assert!(
        text.contains(r#"${CARGO_FEATURES:+--features "$CARGO_FEATURES"}"#),
        "the cargo build line does not pass CARGO_FEATURES as --features"
    );
}

#[test]
fn test_image_workflow_builds_with_raft() {
    let text = read(TEST_IMAGE_WORKFLOW);
    let after = text
        .split("build-args:")
        .nth(1)
        .expect("keep-test-image.yml has no `build-args:` key");
    assert!(
        after.lines().any(|l| l.trim() == "CARGO_FEATURES=raft"),
        "keep-test-image.yml does not pass `CARGO_FEATURES=raft` under build-args"
    );
}

#[test]
fn default_build_has_no_feature() {
    let text = read(DOCKERFILE);
    assert!(
        !text.contains("--features raft"),
        "Dockerfile hardcodes the raft feature; the default build must stay featureless"
    );
}
