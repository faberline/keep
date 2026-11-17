//! Tests for the offline release fixture (`fixture_run`), ported from `verify-release-contract.py`.

use keep_release_gate::contract::find_app;
use keep_release_gate::fixture_run::*;
use keep_release_gate::util::sha256_hex;
use serde_json::json;
use std::fs::File;
use std::path::Path;

const TARGET: &str = "x86_64-unknown-linux-musl";

fn read_entry_names(archive: &Path) -> Vec<(String, u32)> {
    let file = File::open(archive).unwrap();
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    tar.entries()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mode = entry.header().mode().unwrap();
            (path.trim_end_matches('/').to_string(), mode)
        })
        .collect()
}

#[test]
fn run_fixture_keep_runs_two_positive_and_seven_negative() {
    let app = find_app("keep").unwrap();
    assert_eq!(run_fixture(&app), Ok((2, 7)));
}

#[test]
fn make_archive_has_three_entries() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("keep.tar.gz");
    make_archive(&archive, "keep", TARGET, "0.4.13").unwrap();
    let entries = read_entry_names(&archive);
    let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "keep-x86_64-unknown-linux-musl",
            "keep-x86_64-unknown-linux-musl/README.md",
            "keep-x86_64-unknown-linux-musl/keep",
        ]
    );
    assert_eq!(entries[2].1 & 0o777, 0o755);
}

#[test]
fn make_archive_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("a.tar.gz");
    let second = dir.path().join("b.tar.gz");
    make_archive(&first, "keep", TARGET, "0.4.13").unwrap();
    make_archive(&second, "keep", TARGET, "0.4.13").unwrap();
    assert_eq!(
        std::fs::read(first).unwrap(),
        std::fs::read(second).unwrap()
    );
}

#[test]
fn write_json_sorts_nested_keys_and_ends_in_one_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing").join("out.json");
    let value = json!({"b": {"z": 1, "a": 2}, "a": [1]});
    write_json(&path, &value).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        "{\n  \"a\": [\n    1\n  ],\n  \"b\": {\n    \"a\": 2,\n    \"z\": 1\n  }\n}\n"
    );
}

#[test]
fn write_sidecar_writes_the_exact_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("x.bin");
    std::fs::write(&file, b"abc").unwrap();
    let sidecar = write_sidecar(&file).unwrap();
    assert_eq!(sidecar, dir.path().join("x.bin.sha256"));
    let text = std::fs::read_to_string(&sidecar).unwrap();
    assert_eq!(text, format!("{}  x.bin\n", sha256_hex(b"abc")));
}

#[test]
fn run_command_expect_ok_refuses_a_failing_command() {
    let cmd: Vec<String> = vec!["sh".into(), "-c".into(), "exit 1".into()];
    let err = run_command(&cmd, "demo", true, None).unwrap_err();
    assert!(err.0.starts_with("demo: exit 1"), "got: {}", err.0);
}

#[test]
fn run_command_expect_failure_refuses_a_passing_command() {
    let cmd: Vec<String> = vec!["true".into()];
    let err = run_command(&cmd, "demo", false, None).unwrap_err();
    assert!(
        err.0
            .starts_with("negative control unexpectedly passed: demo"),
        "got: {}",
        err.0
    );
}

#[test]
fn run_command_must_print_is_checked() {
    let cmd: Vec<String> = vec!["sh".into(), "-c".into(), "echo hello".into()];
    assert!(run_command(&cmd, "demo", true, Some("hello")).is_ok());
    let err = run_command(&cmd, "demo", true, Some("absent")).unwrap_err();
    assert!(
        err.0
            .starts_with("demo: expected \"absent\" on stdout, got:"),
        "got: {}",
        err.0
    );
}

#[test]
fn run_command_negative_control_accepts_a_failing_command() {
    let cmd: Vec<String> = vec!["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()];
    assert!(run_command(&cmd, "demo", false, None).is_ok());
}

#[test]
fn build_fixture_refuses_the_gcp_backend() {
    let mut app = find_app("keep").unwrap();
    app.gke_backend = "gcp".to_string();
    let dir = tempfile::tempdir().unwrap();
    let err = build_fixture(&app, dir.path()).unwrap_err();
    assert_eq!(err.0, "no fixture for backend gcp");
}

#[test]
fn build_fixture_writes_the_release_files() {
    let app = find_app("keep").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fx = build_fixture(&app, dir.path()).unwrap();
    let names: Vec<String> = std::fs::read_dir(&fx.release_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for expected in [
        "keep-x86_64-unknown-linux-musl.tar.gz",
        "keep-x86_64-unknown-linux-musl.tar.gz.sha256",
        "keep-aarch64-unknown-linux-musl.tar.gz",
        "keep-aarch64-unknown-linux-musl.tar.gz.sha256",
        "spdx-amd64.json",
        "spdx-arm64.json",
        "keep-gke-receipt.json",
        "keep-gke-receipt.json.sha256",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing {expected}: {names:?}"
        );
    }
    assert!(!names
        .iter()
        .any(|n| n.starts_with("final-candidate-manifest")));
}

#[test]
fn commands_use_the_overrides() {
    let app = find_app("keep").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fx = build_fixture(&app, dir.path()).unwrap();

    let cmd = artifacts_command(&fx, &[("commit", "e".repeat(40))]);
    let pos = cmd.iter().position(|a| a == "--commit").unwrap();
    assert_eq!(cmd[pos + 1], "e".repeat(40));
    assert!(cmd[1].ends_with("scripts/release/verify-release-artifacts.sh"));

    let cmd = candidate_local_command(&fx, &[("sidecar", "/x".to_string())]);
    let pos = cmd.iter().position(|a| a == "--manifest-sidecar").unwrap();
    assert_eq!(cmd[pos + 1], "/x");
}

#[test]
fn fixture_version_is_known_for_keep_only() {
    assert_eq!(fixture_version("keep"), Some("0.4.13"));
    assert_eq!(fixture_version("loom"), None);
}

#[test]
fn run_command_refuses_an_empty_command() {
    let err = run_command(&[], "demo", true, None).unwrap_err();
    assert_eq!(err.0, "demo: empty command");
}

#[test]
fn run_command_reports_a_program_that_does_not_start() {
    let cmd: Vec<String> = vec!["/nonexistent/program-x".into()];
    assert!(run_command(&cmd, "demo", true, None).is_err());
}

#[test]
fn build_fixture_refuses_an_app_without_a_fixture_version() {
    let mut app = find_app("keep").unwrap();
    app.name = "loom".to_string();
    let dir = tempfile::tempdir().unwrap();
    let err = build_fixture(&app, dir.path()).unwrap_err();
    assert_eq!(err.0, "no fixture version for loom");
}

#[test]
fn run_fixture_refuses_an_app_without_targets() {
    let mut app = find_app("keep").unwrap();
    app.targets.clear();
    let err = run_fixture(&app).unwrap_err();
    assert_eq!(err.0, "keep has no targets");
}

#[test]
fn write_json_under_a_regular_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    let path = blocker.join("out.json");
    assert!(write_json(&path, &json!({"a": 1})).is_err());
}
