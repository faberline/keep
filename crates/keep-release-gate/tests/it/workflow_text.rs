//! Tests for the workflow text helpers, on a small workflow and on the real release workflows.

use keep_release_gate::contract::{find_app, shared_jobs, ContractError};
use keep_release_gate::workflow_text::{
    action_values, all_run_scripts, assert_actions_pinned, job_block, job_ids, parse_needs,
    permission_pairs, run_script, step_block, top_block, with_values, workflow_inputs,
    workflow_triggers,
};
use std::collections::{BTreeMap, BTreeSet};

const SMALL: &str = r#"name: small
on:
  workflow_dispatch:
    inputs:
      version:
        type: string
      commit_id:
        type: string
  push:
jobs:
  alpha:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
      - name: First
        run: |
          echo one
          echo two
      - name: Second
        run: echo three
  beta-2:
    needs: [alpha, gamma]
    permissions:
      contents: read
      actions: write
    with:
      app: keep
      note: "a: b"
    steps:
      - name: Only
        run: |-
          echo four
"#;

const PIN: &str = "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1";

/// The text of a file under `.github/workflows` of the repository.
fn workflow(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `text` with its first pinned action unpinned to `@v4`.
fn unpin_first_action(text: &str) -> String {
    let re = regex::Regex::new(r"(uses: [^@\s]+)@[0-9a-f]{40}").unwrap();
    let mutated = re.replacen(text, 1, "${1}@v4").into_owned();
    assert_ne!(mutated, text, "no pinned action to unpin");
    mutated
}

/// The error text of a refused call.
fn err_text<T: std::fmt::Debug>(result: Result<T, ContractError>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn top_block_cuts_at_the_next_top_level_key() {
    let on = top_block(SMALL, "on").unwrap();
    assert!(on.starts_with("on:"));
    assert!(on.ends_with("  push:"));
    assert!(!on.contains("jobs:"));
    assert_eq!(
        err_text(top_block(SMALL, "zzz")),
        "top-level key 'zzz' is absent"
    );
}

#[test]
fn triggers_inputs_and_job_ids() {
    assert_eq!(
        workflow_triggers(SMALL).unwrap(),
        vec!["workflow_dispatch", "push"]
    );
    let inputs: BTreeSet<String> = ["version", "commit_id"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(workflow_inputs(SMALL).unwrap(), inputs);
    let jobs: BTreeSet<String> = ["alpha", "beta-2"].iter().map(|s| s.to_string()).collect();
    assert_eq!(job_ids(SMALL).unwrap(), jobs);
}

#[test]
fn job_block_stops_at_the_next_job() {
    let alpha = job_block(SMALL, "alpha").unwrap();
    assert!(alpha.starts_with("  alpha:"));
    assert!(!alpha.contains("beta-2"));
    assert_eq!(
        err_text(job_block(SMALL, "missing")),
        "job missing is absent"
    );
}

#[test]
fn step_block_and_run_script() {
    let alpha = job_block(SMALL, "alpha").unwrap();
    let beta = job_block(SMALL, "beta-2").unwrap();
    let first = step_block(&alpha, "First").unwrap();
    assert_eq!(
        first,
        "      - name: First\n        run: |\n          echo one\n          echo two"
    );
    assert_eq!(
        run_script(&first).unwrap(),
        "          echo one\n          echo two"
    );
    assert_eq!(
        err_text(run_script(&step_block(&alpha, "Second").unwrap())),
        "step must carry exactly one block run script"
    );
    assert_eq!(
        err_text(step_block(&alpha, "Missing")),
        "step 'Missing' must appear exactly once, found 0"
    );
    assert_eq!(
        run_script(&step_block(&beta, "Only").unwrap()).unwrap(),
        "          echo four"
    );
    let dup = "      - name: Dup\n        run: x\n      - name: Dup\n";
    assert_eq!(
        err_text(step_block(dup, "Dup")),
        "step 'Dup' must appear exactly once, found 2"
    );
}

#[test]
fn all_run_scripts_in_order() {
    assert_eq!(
        all_run_scripts(SMALL),
        vec![
            "          echo one\n          echo two".to_string(),
            "echo three".to_string(),
            "          echo four".to_string(),
        ]
    );
}

#[test]
fn action_values_and_pinning() {
    assert_eq!(action_values(SMALL), vec![PIN]);
    assert!(assert_actions_pinned("small", SMALL).is_ok());
    assert_eq!(
        err_text(assert_actions_pinned("lbl", "uses: actions/checkout@v4")),
        "lbl: action not pinned to a 40-hex commit: actions/checkout@v4"
    );
    assert_eq!(
        err_text(assert_actions_pinned("lbl", "- uses: docker/foo")),
        "lbl: action not pinned to a 40-hex commit: docker/foo"
    );
    let upper = format!("uses: x/y@{}", "A".repeat(40));
    assert!(err_text(assert_actions_pinned("lbl", &upper)).starts_with("lbl: action not pinned"));
    let short = format!("uses: x/y@{}", "a".repeat(39));
    assert!(err_text(assert_actions_pinned("lbl", &short)).starts_with("lbl: action not pinned"));
    assert!(assert_actions_pinned("lbl", "uses: ./local").is_ok());
}

#[test]
fn parse_needs_reads_a_bracketed_list() {
    let beta = job_block(SMALL, "beta-2").unwrap();
    assert_eq!(parse_needs(&beta).unwrap(), vec!["alpha", "gamma"]);
    let alpha = job_block(SMALL, "alpha").unwrap();
    assert_eq!(
        err_text(parse_needs(&alpha)),
        "result job must declare a bracketed needs list"
    );
}

#[test]
fn with_values_and_permission_pairs() {
    let beta = job_block(SMALL, "beta-2").unwrap();
    let expected: BTreeMap<String, String> = [("app", "keep"), ("note", "a: b")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(with_values(&beta).unwrap(), expected);
    assert_eq!(
        permission_pairs(&beta).unwrap(),
        vec!["contents: read", "actions: write"]
    );
    let alpha = job_block(SMALL, "alpha").unwrap();
    assert_eq!(
        err_text(with_values(&alpha)),
        "release job must have a with: block"
    );
    assert_eq!(
        err_text(permission_pairs(&alpha)),
        "release job must declare its permissions"
    );
    assert_eq!(
        err_text(with_values("    with:\n      app keep\n")),
        "with: line is not key: value: 'app keep'"
    );
    assert_eq!(
        err_text(with_values("    with:\n      a: 1\n      a: 2\n")),
        "with: key a repeats"
    );
    let empty = with_values("    with:\n      x: \"\"\n").unwrap();
    assert_eq!(empty["x"], "");
}

#[test]
fn candidate_workflow_shape() {
    let text = workflow("keep-release-candidate.yml");
    assert_eq!(workflow_triggers(&text).unwrap(), vec!["workflow_dispatch"]);
    let inputs: BTreeSet<String> = ["version", "commit"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(workflow_inputs(&text).unwrap(), inputs);
    let jobs: BTreeSet<String> = shared_jobs("keep").into_iter().collect();
    assert_eq!(job_ids(&text).unwrap(), jobs);
    let result = job_block(&text, "result").unwrap();
    assert_eq!(
        parse_needs(&result).unwrap(),
        vec![
            "identity",
            "build",
            "keep-release-gates",
            "manifest",
            "ghcr-image-and-attest",
            "verify-candidate",
        ]
    );
    let gates = job_block(&text, "keep-release-gates").unwrap();
    let script = run_script(&step_block(&gates, "Run declared Keep gates").unwrap()).unwrap();
    assert!(script.contains("cargo test --locked -p keep"));
    assert!(script.contains("-p keep-release-gate -- verify-release-contract --app keep"));
    assert!(all_run_scripts(&text).iter().any(|s| s.contains(
        "cargo build --release --locked -p keep --bin keep --target ${{ matrix.target }}"
    )));
    assert!(assert_actions_pinned("candidate", &text).is_ok());
}

#[test]
fn promotion_workflow_shape() {
    let text = workflow("keep-release.yml");
    assert_eq!(workflow_triggers(&text).unwrap(), vec!["workflow_dispatch"]);
    let inputs: BTreeSet<String> = find_app("keep")
        .unwrap()
        .promotion_inputs()
        .into_iter()
        .collect();
    assert_eq!(workflow_inputs(&text).unwrap(), inputs);
    let jobs: BTreeSet<String> = ["release"].iter().map(|s| s.to_string()).collect();
    assert_eq!(job_ids(&text).unwrap(), jobs);
    let job = job_block(&text, "release").unwrap();
    assert_eq!(
        permission_pairs(&job).unwrap(),
        vec![
            "actions: read",
            "attestations: read",
            "contents: write",
            "packages: write",
            "pull-requests: read",
        ]
    );
    let with = with_values(&job).unwrap();
    assert_eq!(with.len(), 9);
    assert_eq!(with["app"], "keep");
    assert_eq!(with["version"], "${{ inputs.version }}");
    assert!(with["compatibility_note"].starts_with("- Compatibility: no HTTP"));
    assert!(assert_actions_pinned("promotion", &text).is_ok());
}

#[test]
fn unpinned_first_action_is_refused() {
    let text = unpin_first_action(&workflow("keep-release-candidate.yml"));
    assert_eq!(
        err_text(assert_actions_pinned("candidate", &text)),
        "candidate: action not pinned to a 40-hex commit: actions/checkout@v4"
    );
}
