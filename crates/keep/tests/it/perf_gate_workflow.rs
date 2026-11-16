//! The nightly competitor perf gate workflow (`.github/workflows/keep-perf-gate.yml`).
//! The workflow is read as text: it must run the ratchet test nightly and by
//! hand on Linux, record the baseline on request, never run on pull requests,
//! and pin every action to a full commit SHA with least privilege.

const WORKFLOW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/keep-perf-gate.yml"
);

fn workflow() -> String {
    std::fs::read_to_string(WORKFLOW).unwrap_or_else(|e| {
        panic!("read {WORKFLOW}: {e} (the nightly perf gate workflow is missing)")
    })
}

#[test]
fn runs_nightly_and_by_hand() {
    let text = workflow();
    assert!(text.contains("schedule:"), "no schedule trigger");
    assert!(text.contains("cron:"), "no cron expression");
    assert!(
        text.contains("workflow_dispatch:"),
        "no manual workflow_dispatch trigger"
    );
}

#[test]
fn never_runs_on_pull_requests() {
    let text = workflow();
    assert!(
        !text.contains("pull_request"),
        "the nightly gate must not run on pull requests"
    );
    assert!(
        !text.contains("push:"),
        "the nightly gate must not run on push"
    );
    assert!(
        !text.lines().any(|l| l == "  gate:"),
        "the job id must not be `gate`: that is the required check"
    );
    assert!(
        !text.contains("name: gate"),
        "the job name must not be `gate`: that is the required check"
    );
}

#[test]
fn runs_the_ratchet_test_on_the_bench_image() {
    let text = workflow();
    assert!(
        text.contains("docker build -f Dockerfile.bench -t keep-bench:gate ."),
        "the bench image is not built"
    );
    assert!(
        text.contains("--test perf_gate_vs_kv competitive_perf_gate -- --ignored --exact"),
        "the ratchet test is not run as the ignored exact test"
    );
}

#[test]
fn can_record_the_baseline() {
    let text = workflow();
    assert!(text.contains("KEEP_PERF_RECORD"), "no record switch");
    assert!(
        text.contains("KEEP_PERF_HOST_CLASS: gha-ubuntu-latest"),
        "host class must be fixed to gha-ubuntu-latest"
    );
    assert!(
        text.contains("actions/upload-artifact@"),
        "the recorded baseline is not uploaded"
    );
    assert!(
        text.contains("crates/keep/tests/perf-baseline.json"),
        "the baseline file is not the one the ratchet reads"
    );
}

#[test]
fn gates_one_core_cells_on_the_four_cpu_runner() {
    let text = workflow();
    assert!(
        text.contains("runs-on: ubuntu-latest"),
        "the gate must run on ubuntu-latest (4 CPUs)"
    );
    assert!(
        text.contains("KEEP_PERF_CORES: \"1\""),
        "a 4-CPU runner cannot host the 4-core cell (needs 6 CPUs): set KEEP_PERF_CORES to 1"
    );
}

#[test]
fn least_privilege_and_pinned_actions() {
    let text = workflow();
    assert!(
        text.contains("permissions:\n  contents: read"),
        "top-level permissions must be contents: read only"
    );
    for line in text.lines().filter(|l| l.contains("uses:")) {
        let pin = line
            .split('@')
            .nth(1)
            .unwrap_or_else(|| panic!("action is not pinned with @: {line}"));
        let sha: String = pin.chars().take(40).collect();
        assert!(
            sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
            "action is not pinned to a 40-hex SHA: {line}"
        );
    }
}

#[test]
fn gate_fails_on_regression_above_threshold() {
    let text = workflow();
    let ratchet = text
        .lines()
        .find(|l| l.contains("competitive_perf_gate -- --ignored --exact"))
        .expect("the ratchet test line is missing");
    assert!(
        !ratchet.contains("|| true"),
        "the ratchet line swallows its failure with `|| true`: a ratchet failure must fail the job"
    );
    assert!(
        !ratchet.contains("continue-on-error"),
        "the ratchet line uses continue-on-error: a ratchet failure must fail the job"
    );
    assert!(
        !text.contains("continue-on-error"),
        "the workflow uses continue-on-error: a ratchet failure must fail the job"
    );
    assert!(
        !text.contains("|| true"),
        "the workflow uses `|| true`: a ratchet failure must fail the job"
    );
}
