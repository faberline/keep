//! The rolling upgrade baseline: CI builds keep at the commit named in
//! `scripts/previous-keep-sha` and runs the `rolling_upgrade::` tests against it.
//! The files are read as text.

const BASELINE_SHA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/previous-keep-sha"
);
const BUILD_SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/build-previous-keep.sh"
);
const CI_WORKFLOW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/ci.yml"
);

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn baseline_sha_file_is_one_full_sha() {
    let text = read(BASELINE_SHA);
    let sha = text.trim();
    assert_eq!(
        sha.len(),
        40,
        "scripts/previous-keep-sha must hold one 40-character SHA, got {sha:?}"
    );
    assert!(
        sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "the SHA must be lowercase hex: {sha:?}"
    );
    assert_eq!(sha, "a6f8eb204bde8641a95eedb231a70ff946d87052");
    assert!(
        text.ends_with('\n'),
        "scripts/previous-keep-sha must end with a newline"
    );
}

#[test]
fn ci_builds_the_baseline_and_gate_needs_it() {
    let text = read(CI_WORKFLOW);
    assert!(
        text.contains("scripts/build-previous-keep.sh"),
        "ci.yml does not run scripts/build-previous-keep.sh"
    );
    assert!(
        text.contains("rolling_upgrade::"),
        "ci.yml does not run the rolling_upgrade:: tests"
    );
    let lines: Vec<&str> = text.lines().collect();
    let gate_idx = lines
        .iter()
        .position(|l| *l == "    name: gate")
        .expect("ci.yml has no job `gate` (name: gate)");
    let needs = lines[gate_idx..]
        .iter()
        .find(|l| l.trim_start().starts_with("needs:"))
        .expect("the gate job has no needs: line");
    assert!(
        needs.contains("test-rolling-upgrade"),
        "the gate job does not need test-rolling-upgrade: {needs}"
    );
}

#[test]
fn build_script_copies_to_target_previous() {
    let text = read(BUILD_SCRIPT);
    assert!(
        text.contains("target/previous/keep"),
        "scripts/build-previous-keep.sh does not copy the binary to target/previous/keep"
    );
    assert!(
        text.contains("--features raft"),
        "scripts/build-previous-keep.sh does not build with --features raft"
    );
}
