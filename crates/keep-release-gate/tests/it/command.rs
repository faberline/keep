//! Tests for the `release-gate` command: `command::run` and the `release-gate` binary.

use keep_release_gate::command::{run, verify_release_contract, USAGE};
use std::path::{Path, PathBuf};

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

fn parity() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parity")
}

/// The arguments of a passing `receipt` run that writes to `output`.
fn receipt_args(output: &str) -> Vec<String> {
    let parity = parity();
    let mut list = args(&["receipt", "--app", "keep", "--backend", "gke-acceptance"]);
    list.push("--candidate-manifest".to_string());
    list.push(
        parity
            .join("final-candidate-manifest.json")
            .display()
            .to_string(),
    );
    list.push("--output".to_string());
    list.push(output.to_string());
    list.push("--gh-run".to_string());
    list.push(parity.join("gh-run.json").display().to_string());
    list.push("--evidence-dir".to_string());
    list.push(parity.join("evidence").display().to_string());
    list
}

/// The list without `option` and its value.
fn without(mut list: Vec<String>, option: &str) -> Vec<String> {
    if let Some(index) = list.iter().position(|item| item == option) {
        list.drain(index..index + 2);
    }
    list
}

/// The list with the value of `option` replaced by `value`.
fn with_value(mut list: Vec<String>, option: &str, value: &str) -> Vec<String> {
    if let Some(index) = list.iter().position(|item| item == option) {
        list[index + 1] = value.to_string();
    }
    list
}

#[test]
fn receipt_command_writes_the_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path().join("keep-gke-receipt.json");
    let output_text = output.display().to_string();

    let (code, stdout, stderr) = run(&receipt_args(&output_text));

    assert_eq!(
        (code, stdout, stderr),
        (
            0,
            format!("GKE receipt written: {output_text}\n"),
            String::new()
        )
    );
    assert_eq!(
        std::fs::read(&output).unwrap(),
        std::fs::read(parity().join("keep-gke-receipt.json")).unwrap()
    );
}

#[test]
fn missing_option_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp
        .path()
        .join("keep-gke-receipt.json")
        .display()
        .to_string();

    let list = without(receipt_args(&output), "--output");

    assert_eq!(
        run(&list),
        (
            2,
            String::new(),
            "release-gate receipt: --output is required\n".to_string()
        )
    );
}

#[test]
fn missing_app_is_reported_first() {
    assert_eq!(
        run(&args(&["receipt"])),
        (
            2,
            String::new(),
            "release-gate receipt: --app is required\n".to_string()
        )
    );
}

#[test]
fn gcp_backend_is_refused_with_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp
        .path()
        .join("keep-gke-receipt.json")
        .display()
        .to_string();

    let list = with_value(receipt_args(&output), "--backend", "gcp");

    assert_eq!(
        run(&list),
        (
            1,
            String::new(),
            "GKE receipt refused: keep proves GKE acceptance with backend gke-acceptance, not gcp\n"
                .to_string()
        )
    );
}

#[test]
fn unknown_subcommand_exits_2() {
    assert_eq!(
        run(&args(&["bogus"])),
        (
            2,
            String::new(),
            "release-gate: unknown subcommand bogus\n".to_string()
        )
    );
}

#[test]
fn no_argument_prints_usage() {
    assert_eq!(run(&[]), (2, String::new(), format!("{USAGE}\n")));
}

#[test]
fn invalid_app_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp
        .path()
        .join("keep-gke-receipt.json")
        .display()
        .to_string();

    let list = with_value(receipt_args(&output), "--app", "loom");

    assert_eq!(
        run(&list),
        (
            2,
            String::new(),
            "release-gate receipt: invalid choice: loom\n".to_string()
        )
    );
}

#[test]
fn invalid_backend_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp
        .path()
        .join("keep-gke-receipt.json")
        .display()
        .to_string();

    let list = with_value(receipt_args(&output), "--backend", "nope");

    assert_eq!(
        run(&list),
        (
            2,
            String::new(),
            "release-gate receipt: invalid choice: nope\n".to_string()
        )
    );
}

#[test]
fn unknown_option_exits_2() {
    assert_eq!(
        run(&args(&["receipt", "--bogus", "x"])),
        (
            2,
            String::new(),
            "release-gate receipt: unknown option --bogus\n".to_string()
        )
    );
}

#[test]
fn option_without_value_exits_2() {
    assert_eq!(
        run(&args(&["receipt", "--app"])),
        (
            2,
            String::new(),
            "release-gate receipt: --app needs a value\n".to_string()
        )
    );
}

#[test]
fn item_that_is_not_an_option_exits_2() {
    assert_eq!(
        run(&args(&["receipt", "keep"])),
        (
            2,
            String::new(),
            "release-gate receipt: unknown option keep\n".to_string()
        )
    );
}

#[test]
fn wrong_output_name_is_refused_with_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path().join("receipt.json").display().to_string();

    assert_eq!(
        run(&receipt_args(&output)),
        (
            1,
            String::new(),
            "GKE receipt refused: output must be named keep-gke-receipt.json\n".to_string()
        )
    );
}

#[test]
fn optional_options_are_accepted_and_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp
        .path()
        .join("keep-gke-receipt.json")
        .display()
        .to_string();

    let mut list = receipt_args(&output);
    list.extend(args(&[
        "--run",
        "ignored",
        "--images",
        "ignored",
        "--acceptance",
        "ignored",
        "--cleanup",
        "ignored",
    ]));

    assert_eq!(run(&list).0, 0);
}

#[test]
fn binary_runs() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_release-gate"))
        .arg("receipt")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "release-gate receipt: --app is required\n"
    );
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn verify_release_contract_passes_on_repository() {
    let result = verify_release_contract(&args(&["--app", "keep", "--static-only"]), &repo_root());
    assert_eq!(
        result,
        (
            0,
            "release contract passed: keep\n".to_string(),
            String::new()
        )
    );
}

#[test]
fn verify_release_contract_usage_errors() {
    let root = repo_root();
    let cases: [(&[&str], &str); 5] = [
        (
            &[],
            "release-gate verify-release-contract: --app is required\n",
        ),
        (
            &["--app", "keep"],
            "release-gate verify-release-contract: --static-only is required\n",
        ),
        (
            &["--app", "sift", "--static-only"],
            "release-gate verify-release-contract: invalid choice: sift\n",
        ),
        (
            &["--bogus"],
            "release-gate verify-release-contract: unknown option --bogus\n",
        ),
        (
            &["--app"],
            "release-gate verify-release-contract: --app needs a value\n",
        ),
    ];
    for (items, stderr) in cases {
        assert_eq!(
            verify_release_contract(&args(items), &root),
            (2, String::new(), stderr.to_string()),
            "args {items:?}"
        );
    }
}

#[test]
fn verify_release_contract_refusal_exit_1() {
    let source = repo_root();
    let dir = tempfile::tempdir().unwrap();
    for relative in [
        ".github/workflows/keep-release.yml",
        ".github/workflows/keep-release-candidate.yml",
        "scripts/release/apps.sh",
    ] {
        let target = dir.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let text = std::fs::read_to_string(source.join(relative)).unwrap();
        let text = if relative == ".github/workflows/keep-release.yml" {
            let from = "service-release.yml@eddb20c39f82655f1b41348230f5bb0925827122";
            assert!(text.contains(from));
            text.replacen(from, "service-release.yml@main", 1)
        } else {
            text
        };
        std::fs::write(&target, text).unwrap();
    }
    assert_eq!(
        verify_release_contract(&args(&["--app", "keep", "--static-only"]), dir.path()),
        (
            1,
            String::new(),
            "verify-release-contract: refused: keep promotion: the only uses must be faberline/aw service-release.yml pinned to a 40-hex commit, got ['faberline/aw/.github/workflows/service-release.yml@main']\n".to_string(),
        )
    );
}

/// Copy the directory `from` into `to`, recursively.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
fn static_only_refuses_a_changed_apps_table() {
    let source = repo_root();
    let dir = tempfile::tempdir().unwrap();
    copy_dir(
        &source.join(".github/workflows"),
        &dir.path().join(".github/workflows"),
    );
    copy_dir(
        &source.join("scripts/release"),
        &dir.path().join("scripts/release"),
    );
    let apps_sh = dir.path().join("scripts/release/apps.sh");
    let text = std::fs::read_to_string(&apps_sh).unwrap();
    let from = "printf '%s\\n' x86_64-unknown-linux-musl aarch64-unknown-linux-musl";
    assert_eq!(text.matches(from).count(), 1);
    std::fs::write(
        &apps_sh,
        text.replacen(from, "printf '%s\\n' x86_64-unknown-linux-musl", 1),
    )
    .unwrap();
    assert_eq!(
        verify_release_contract(&args(&["--app", "keep", "--static-only"]), dir.path()),
        (
            1,
            String::new(),
            "verify-release-contract: refused: apps.sh release_app_targets keep = ['x86_64-unknown-linux-musl'], this table says ['x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl']\n".to_string(),
        )
    );
}
