//! Tests for the mutation controls, `check_everything` and `self_test` (ported from `verify-release-contract.py`).

use keep_release_gate::candidate_contract::check_candidate;
use keep_release_gate::contract::{find_app, ContractError};
use keep_release_gate::fixture_run::repo_root;
use keep_release_gate::promotion_contract::{check_promotion, read_workflow};
use keep_release_gate::self_test::*;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    repo_root()
}

fn err_message<T: std::fmt::Debug>(result: Result<T, ContractError>) -> String {
    match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(error) => error.0,
    }
}

#[test]
fn replace_all_absent_anchor() {
    assert_eq!(
        err_message(replace_all("abc", "z", "y", "L")),
        "L: mutation anchor is absent: 'z'"
    );
    assert_eq!(replace_all("aXbX", "X", "Y", "L").unwrap(), "aYbY");
}

#[test]
fn replace_once_is_shared() {
    assert_eq!(
        err_message(replace_once_checked("aXbXc", "X", "Y", "L")),
        "L: mutation anchor must appear exactly once: 'X' (2)"
    );
    assert_eq!(replace_once_checked("aXbc", "X", "Y", "L").unwrap(), "aYbc");
}

#[test]
fn unpin_first_action_cases() {
    let pin = "0123456789abcdef0123456789abcdef01234567";
    let input = format!("- uses: actions/checkout@{pin}\n- uses: other/x@{pin}\n");
    let expected = format!("- uses: actions/checkout@v4\n- uses: other/x@{pin}\n");
    assert_eq!(unpin_first_action(&input, "L").unwrap(), expected);
    assert_eq!(
        err_message(unpin_first_action("- uses: actions/checkout@v4\n", "L")),
        "L: no pinned action to unpin"
    );
}

#[test]
fn unpin_shared_workflow_cases() {
    let pin = "0123456789abcdef0123456789abcdef01234567";
    let input = format!("uses: faberline/aw/.github/workflows/service-release.yml@{pin}\n");
    assert_eq!(
        unpin_shared_workflow(&input, "L").unwrap(),
        "uses: faberline/aw/.github/workflows/service-release.yml@main\n"
    );
    assert_eq!(
        err_message(unpin_shared_workflow("uses: x\n", "L")),
        "L: no pinned shared workflow to unpin"
    );
}

#[test]
fn expect_static_failure_cases() {
    // The action passes, so the control is not refused: this is the negative test.
    assert_eq!(
        err_message(expect_static_failure("L", || Ok(()))),
        "negative control unexpectedly passed: L"
    );
    // The action is refused, so the control holds.
    assert_eq!(expect_static_failure("L", || fail_with("boom")), Ok(()));
}

fn fail_with(message: &str) -> Result<(), ContractError> {
    Err(ContractError(message.to_string()))
}

#[test]
fn watched_files_lists_the_checked_files() {
    let names: Vec<String> = watched_files()
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect();
    assert_eq!(
        names,
        [
            "scripts/release/apps.sh",
            "scripts/release/verify-release-candidate.sh",
            "scripts/release/verify-release-artifacts.sh",
            ".github/workflows/keep-release-candidate.yml",
            ".github/workflows/keep-release.yml",
            ".github/workflows/gke-acceptance.yml",
        ]
    );
}

#[test]
fn digests_and_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one").unwrap();
    std::fs::write(dir.path().join("b.txt"), "two").unwrap();
    let paths = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
    let before = digests(dir.path(), &paths).unwrap();
    assert_eq!(
        changed_files(&paths, &before, &before),
        Vec::<String>::new()
    );

    std::fs::write(dir.path().join("b.txt"), "three").unwrap();
    let after = digests(dir.path(), &paths).unwrap();
    assert_eq!(changed_files(&paths, &before, &after), ["b.txt"]);

    assert_eq!(
        err_message(digests(dir.path(), &[PathBuf::from("nope.txt")])),
        "missing file: nope.txt"
    );
}

#[test]
fn check_everything_passes_on_the_repository() {
    assert_eq!(
        check_everything(&root()).unwrap(),
        "release contract passed: keep (1 apps), shared tables"
    );
}

#[test]
fn candidate_controls_all_fail_the_check() {
    let app = find_app("keep").unwrap();
    let text = read_workflow(&root(), &app.candidate_workflow()).unwrap();
    let controls = candidate_controls(&app, &text);
    let names: Vec<&str> = controls.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "keep candidate: commit input renamed",
            "keep candidate: result job renamed",
            "keep candidate: action unpinned",
            "keep candidate: manifest schema bumped silently",
            "keep candidate: verifier downgraded to local mode",
            "keep candidate: commit identity proof dropped",
            "keep candidate: build result binding faked",
            "keep candidate: main-only guard dropped",
            "keep candidate: extra trigger added",
            "keep candidate: stale target archive still downloaded",
            "keep candidate: manifest loop drops a target",
            "keep candidate: build matrix row dropped",
        ]
    );
    for (name, control) in &controls {
        let mutated = control().unwrap_or_else(|error| panic!("{name}: {}", error.0));
        assert_ne!(mutated, text, "{name} did not change the text");
        assert!(
            check_candidate(&app, &mutated).is_err(),
            "{name} was not refused"
        );
    }
}

#[test]
fn promotion_controls_all_fail_the_check() {
    let app = find_app("keep").unwrap();
    let text = read_workflow(&root(), &app.promotion_workflow()).unwrap();
    let controls = promotion_controls(&root(), &app, &text).unwrap();
    let names: Vec<&str> = controls.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "keep promotion: tag push trigger added",
            "keep promotion: candidate run id input dropped",
            "keep promotion: release job renamed",
            "keep promotion: shared workflow unpinned",
            "keep promotion: shared workflow from another path",
            "keep promotion: inline step added",
            "keep promotion: extra permission",
            "keep promotion: app input changed",
            "keep promotion: dispatch input not passed through",
            "keep promotion: compatibility line reworded",
            "keep promotion: verify script overridden",
        ]
    );
    for (name, control) in &controls {
        let mutated = control().unwrap_or_else(|error| panic!("{name}: {}", error.0));
        assert_ne!(mutated, text, "{name} did not change the text");
        assert!(
            check_promotion(&root(), &app, &mutated).is_err(),
            "{name} was not refused"
        );
    }
}

#[test]
fn promotion_controls_need_the_compatibility_line() {
    let app = find_app("keep").unwrap();
    let text = read_workflow(&root(), &app.promotion_workflow()).unwrap();
    let empty = tempfile::tempdir().unwrap();
    assert!(promotion_controls(empty.path(), &app, &text).is_err());
}

#[test]
fn control_setup_failure_is_returned() {
    let app = find_app("keep").unwrap();
    let controls = candidate_controls(&app, "no anchors here");
    let (_, first) = &controls[0];
    assert_eq!(
        err_message(first()),
        "keep candidate: mutation anchor must appear exactly once: '      commit:\\n' (0)"
    );
}

#[test]
fn self_test_passes_on_the_repository() {
    assert_eq!(
        self_test(&root()).unwrap(),
        "release contract self-test passed: 5 positive checks, 36 negative controls refused, fixtures keep"
    );
}

#[test]
fn self_test_refuses_a_missing_workflow() {
    let empty = tempfile::tempdir().unwrap();
    let message = err_message(self_test(Path::new(empty.path())));
    assert!(
        message.starts_with("missing file:"),
        "unexpected message: {message}"
    );
}
