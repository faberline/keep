//! The offline release fixture of the contract check: it builds a fake candidate and release, then runs the shell verifiers on it. Port of `build_fixture` and `run_fixture` in `verify-release-contract.py`.

use crate::candidate::IMAGE_OWNER;
use crate::contract::{fail, App, ContractError, Result};
use crate::fixtures;
use crate::receipt::{make_receipt, ReceiptInputs};
use crate::refusal::Refusal;
use crate::util::sha256_hex;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Map, Value};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository that the fixture pretends to release.
pub const REPO: &str = "faberline/keep";
/// The banner that the shell verifiers print in fixture mode.
pub const FIXTURE_BANNER: &str = "LOCAL FIXTURE ONLY";
/// The apps that have a fixture.
pub const FIXTURE_APPS: [&str; 1] = ["keep"];
/// The version of the `keep` fixture.
pub const FIXTURE_VERSION: &str = "0.4.13";
/// The shell verifier of the release artifacts, from the repository root.
pub const ARTIFACT_VERIFIER: &str = "scripts/release/verify-release-artifacts.sh";
/// The shell verifier of the candidate, from the repository root.
pub const CANDIDATE_VERIFIER: &str = "scripts/release/verify-release-candidate.sh";

/// The version of the fixture of `app`, when it has one.
pub fn fixture_version(app: &str) -> Option<&'static str> {
    (app == "keep").then_some(FIXTURE_VERSION)
}

/// The repository root: the working directory of the shell verifiers.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn io(error: io::Error) -> ContractError {
    ContractError(error.to_string())
}

fn refuse_fixture(app: &str, message: impl std::fmt::Display) -> ContractError {
    ContractError(format!(
        "{app} receipt maker refused the fixture: {message}"
    ))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn file_sha256(path: &Path) -> io::Result<String> {
    Ok(sha256_hex(&std::fs::read(path)?))
}

/// Copy of `value` with every object's keys in sorted order, at every level.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (key, item) in entries {
                out.insert(key.clone(), sorted(item));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Write `value` as JSON with 2-space indent, keys sorted at every level, and one trailing newline. Creates the parent directory.
pub fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(&sorted(value))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    text.push('\n');
    std::fs::write(path, text)
}

/// Write `<file>.sha256` next to `path` with the line `<sha256>  <file name>`, and return its path.
pub fn write_sidecar(path: &Path) -> io::Result<PathBuf> {
    let name = file_name_of(path);
    let sidecar = path.with_file_name(format!("{name}.sha256"));
    let digest = file_sha256(path)?;
    std::fs::write(&sidecar, format!("{digest}  {name}\n"))?;
    Ok(sidecar)
}

/// Append one entry with fixed metadata, so that two runs give the same bytes.
fn append_entry(
    builder: &mut tar::Builder<GzEncoder<std::fs::File>>,
    path: &str,
    mode: u32,
    kind: tar::EntryType,
    data: &[u8],
) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(data.len() as u64);
    builder.append_data(&mut header, path, data)
}

/// Write a `.tar.gz` with the directory `<app>-<target>`, its `README.md` and the executable `<app>`.
pub fn make_archive(path: &Path, app: &str, target: &str, version: &str) -> io::Result<()> {
    let top = format!("{app}-{target}");
    let binary = format!("#!/bin/sh\nprintf '%s %s\\n' '{app}' '{version}'\n");
    let readme = format!("{app} release fixture for {target}\n");
    let file = std::fs::File::create(path)?;
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    append_entry(
        &mut builder,
        &format!("{top}/"),
        0o755,
        tar::EntryType::Directory,
        &[],
    )?;
    append_entry(
        &mut builder,
        &format!("{top}/README.md"),
        0o644,
        tar::EntryType::Regular,
        readme.as_bytes(),
    )?;
    append_entry(
        &mut builder,
        &format!("{top}/{app}"),
        0o755,
        tar::EntryType::Regular,
        binary.as_bytes(),
    )?;
    builder.into_inner()?.finish()?;
    Ok(())
}

/// Run `cmd` in the repository root. With `expect_ok`, it must pass and print `must_print`; otherwise it must fail, and the refusal is printed.
pub fn run_command(
    cmd: &[String],
    label: &str,
    expect_ok: bool,
    must_print: Option<&str>,
) -> Result<()> {
    let Some((program, args)) = cmd.split_first() else {
        return fail(format!("{label}: empty command"));
    };
    let output = Command::new(program)
        .args(args)
        .current_dir(repo_root())
        .output()
        .map_err(io)?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if expect_ok {
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return fail(format!("{label}: exit {code}\n{stdout}{stderr}"));
        }
        if let Some(text) = must_print {
            if !stdout.contains(text) {
                return fail(format!(
                    "{label}: expected {text:?} on stdout, got:\n{stdout}"
                ));
            }
        }
        return Ok(());
    }
    if output.status.success() {
        return fail(format!(
            "negative control unexpectedly passed: {label}\n{stdout}"
        ));
    }
    let last = stderr
        .trim()
        .lines()
        .last()
        .or_else(|| stdout.trim().lines().last())
        .unwrap_or("<no output>");
    println!("negative control refused: {label}: {last}");
    Ok(())
}

/// The files and values of one fixture run.
#[derive(Debug)]
pub struct Fixture {
    pub app: App,
    pub version: String,
    pub commit: String,
    pub run_id: String,
    pub attempt: String,
    pub candidate_dir: PathBuf,
    pub release_dir: PathBuf,
    pub manifest: PathBuf,
    pub receipt: PathBuf,
    pub sidecar: PathBuf,
}

/// Build the candidate and the release of `app` under `root`, with a receipt from the GKE acceptance maker.
pub fn build_fixture(app: &App, root: &Path) -> Result<Fixture> {
    let Some(version) = fixture_version(&app.name) else {
        return fail(format!("no fixture version for {}", app.name));
    };
    if app.gke_backend != "gke-acceptance" {
        return fail(format!("no fixture for backend {}", app.gke_backend));
    }
    let version = version.to_string();
    let commit = "f".repeat(40);
    let run_id = "424242".to_string();
    let attempt = "2".to_string();
    let candidate_dir = root.join("candidate");
    let release_dir = root.join("release");
    std::fs::create_dir_all(&candidate_dir).map_err(io)?;
    std::fs::create_dir_all(&release_dir).map_err(io)?;

    let mut manifest = fixtures::candidate(&app.name, &version, &commit, &run_id, &attempt);
    let mut artifacts = Vec::new();
    for target in &app.targets {
        let archive = candidate_dir.join(format!("{}-{target}.tar.gz", app.name));
        make_archive(&archive, &app.name, target, &version).map_err(io)?;
        let sidecar = write_sidecar(&archive).map_err(io)?;
        artifacts.push(json!({
            "target": target,
            "archive": file_name_of(&archive),
            "archive_sha256": file_sha256(&archive).map_err(io)?,
            "sidecar": file_name_of(&sidecar),
            "sidecar_sha256": file_sha256(&sidecar).map_err(io)?,
        }));
    }
    manifest["artifacts"] = Value::Array(artifacts);
    for arch in ["amd64", "arm64"] {
        let spdx = candidate_dir.join(format!("spdx-{arch}.json"));
        write_json(
            &spdx,
            &json!({"spdxVersion": "SPDX-2.3", "name": format!("{}-{arch}", app.name), "packages": []}),
        )
        .map_err(io)?;
        manifest["sboms"][arch] = json!({
            "file": format!("spdx-{arch}.json"),
            "sha256": file_sha256(&spdx).map_err(io)?,
        });
    }
    let manifest_path = candidate_dir.join("final-candidate-manifest.json");
    write_json(&manifest_path, &manifest).map_err(io)?;
    write_sidecar(&manifest_path).map_err(io)?;

    let root_digest = manifest["image"]["root_digest"].as_str().unwrap_or("");
    let image = format!("{IMAGE_OWNER}/{}@{root_digest}", app.name);
    let receipt = root.join(&app.receipt_name);
    let evidence = root.join("evidence");
    std::fs::create_dir_all(&evidence).map_err(io)?;
    let gh_run = evidence.join("gh-run.json");
    let bundle = evidence.join("bundle");
    write_json(
        &gh_run,
        &fixtures::gke_acceptance(&app.name, &commit, &image),
    )
    .map_err(|error| refuse_fixture(&app.name, error))?;
    fixtures::write_harness_evidence(&app.name, &bundle, &image)
        .map_err(|error| refuse_fixture(&app.name, error))?;
    make_receipt(
        &app.name,
        "gke-acceptance",
        &manifest_path,
        &receipt,
        &ReceiptInputs {
            gh_run: Some(&gh_run),
            evidence_dir: Some(&bundle),
            ..Default::default()
        },
    )
    .map_err(|error: Refusal| refuse_fixture(&app.name, error.0))?;

    // `make_receipt` writes the sidecar next to the receipt.
    let sidecar = receipt.with_file_name(format!("{}.sha256", file_name_of(&receipt)));

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&candidate_dir)
        .map_err(io)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<_>>()
        .map_err(io)?;
    entries.sort();
    for path in entries {
        let name = file_name_of(&path);
        if name.starts_with("final-candidate-manifest") {
            continue;
        }
        std::fs::copy(&path, release_dir.join(&name)).map_err(io)?;
    }
    for path in [&receipt, &sidecar] {
        std::fs::copy(path, release_dir.join(file_name_of(path))).map_err(io)?;
    }

    Ok(Fixture {
        app: app.clone(),
        version,
        commit,
        run_id,
        attempt,
        candidate_dir,
        release_dir,
        manifest: manifest_path,
        receipt,
        sidecar,
    })
}

/// The value of `key` in `overrides`, or `default` when there is none.
fn over(overrides: &[(&str, String)], key: &str, default: String) -> String {
    overrides
        .iter()
        .rev()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value.clone())
        .unwrap_or(default)
}

/// The command line of the release artifacts verifier in fixture mode. Overrides: `commit`, `run_id`, `attempt`, `candidate_dir`, `release_dir`, `receipt`, `sidecar`.
pub fn artifacts_command(fx: &Fixture, overrides: &[(&str, String)]) -> Vec<String> {
    let verifier = repo_root().join(ARTIFACT_VERIFIER);
    let cmd: Vec<String> = vec![
        "bash".into(),
        path_string(&verifier),
        "--app".into(),
        fx.app.name.clone(),
        "--repo".into(),
        REPO.into(),
        "--tag".into(),
        format!("{}@{}", fx.app.name, fx.version),
        "--commit".into(),
        over(overrides, "commit", fx.commit.clone()),
        "--candidate-run-id".into(),
        over(overrides, "run_id", fx.run_id.clone()),
        "--candidate-run-attempt".into(),
        over(overrides, "attempt", fx.attempt.clone()),
        "--mode".into(),
        "fixture".into(),
        "--candidate-receipt-dir".into(),
        over(overrides, "candidate_dir", path_string(&fx.candidate_dir)),
        "--release-assets-dir".into(),
        over(overrides, "release_dir", path_string(&fx.release_dir)),
        "--gke-receipt".into(),
        over(overrides, "receipt", path_string(&fx.receipt)),
        "--gke-receipt-sidecar".into(),
        over(overrides, "sidecar", path_string(&fx.sidecar)),
    ];
    cmd
}

/// The command line of the candidate verifier in local mode. Overrides: `manifest`, `sidecar`, `commit`.
pub fn candidate_local_command(fx: &Fixture, overrides: &[(&str, String)]) -> Vec<String> {
    let verifier = repo_root().join(CANDIDATE_VERIFIER);
    let default_sidecar = fx
        .manifest
        .with_file_name(format!("{}.sha256", file_name_of(&fx.manifest)));
    let cmd: Vec<String> = vec![
        "bash".into(),
        path_string(&verifier),
        "--app".into(),
        fx.app.name.clone(),
        "--repo".into(),
        REPO.into(),
        "--version".into(),
        fx.version.clone(),
        "--commit".into(),
        over(overrides, "commit", fx.commit.clone()),
        "--run-id".into(),
        fx.run_id.clone(),
        "--run-attempt".into(),
        fx.attempt.clone(),
        "--manifest".into(),
        over(overrides, "manifest", path_string(&fx.manifest)),
        "--manifest-sidecar".into(),
        over(overrides, "sidecar", path_string(&default_sidecar)),
        "--artifacts-dir".into(),
        path_string(&fx.candidate_dir),
        "--mode".into(),
        "local".into(),
    ];
    cmd
}

/// Copy the files of `from` (no subdirectories) into `to`, creating it.
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        std::fs::copy(entry.path(), to.join(entry.file_name()))?;
    }
    Ok(())
}

/// Run the 2 positive and 7 negative checks of the fixture of `app`. Returns the counts of the positive and the negative runs.
pub fn run_fixture(app: &App) -> Result<(usize, usize)> {
    let name = app.name.as_str();
    let tmp = tempfile::Builder::new()
        .prefix(&format!("release-contract-{name}-"))
        .tempdir()
        .map_err(io)?;
    let root = tmp.path();
    let t0 = match app.targets.first() {
        Some(target) => target.clone(),
        None => return fail(format!("{name} has no targets")),
    };
    let fx = build_fixture(app, root)?;
    let mut positives = 0;
    let mut negatives = 0;

    run_command(
        &candidate_local_command(&fx, &[]),
        &format!("{name} candidate fixture (local mode)"),
        true,
        Some(FIXTURE_BANNER),
    )?;
    positives += 1;
    run_command(
        &artifacts_command(&fx, &[]),
        &format!("{name} release fixture"),
        true,
        Some(FIXTURE_BANNER),
    )?;
    positives += 1;

    run_command(
        &artifacts_command(&fx, &[("commit", "e".repeat(40))]),
        &format!("{name} fixture: commit differs from the candidate"),
        false,
        None,
    )?;
    negatives += 1;
    run_command(
        &artifacts_command(&fx, &[("attempt", "1".to_string())]),
        &format!("{name} fixture: candidate run attempt differs"),
        false,
        None,
    )?;
    negatives += 1;

    let bad = root.join("bad-result");
    std::fs::create_dir_all(&bad).map_err(io)?;
    let mut flipped: Value =
        serde_json::from_str(&std::fs::read_to_string(&fx.receipt).map_err(io)?)
            .map_err(|error| ContractError(error.to_string()))?;
    flipped["result"] = json!("failed");
    let bad_receipt = bad.join(&app.receipt_name);
    write_json(&bad_receipt, &flipped).map_err(io)?;
    let bad_sidecar = write_sidecar(&bad_receipt).map_err(io)?;
    run_command(
        &artifacts_command(
            &fx,
            &[
                ("receipt", path_string(&bad_receipt)),
                ("sidecar", path_string(&bad_sidecar)),
            ],
        ),
        &format!("{name} fixture: receipt result is not passed"),
        false,
        None,
    )?;
    negatives += 1;

    let tampered = root.join("tampered");
    std::fs::create_dir_all(&tampered).map_err(io)?;
    let tampered_receipt = tampered.join(&app.receipt_name);
    let receipt_text = std::fs::read_to_string(&fx.receipt).map_err(io)?;
    std::fs::write(&tampered_receipt, format!("{receipt_text}\n")).map_err(io)?;
    run_command(
        &artifacts_command(&fx, &[("receipt", path_string(&tampered_receipt))]),
        &format!("{name} fixture: receipt bytes differ from its sidecar"),
        false,
        None,
    )?;
    negatives += 1;

    let missing = root.join("missing-asset");
    copy_dir(&fx.release_dir, &missing).map_err(io)?;
    std::fs::remove_file(missing.join(format!("{name}-{t0}.tar.gz.sha256"))).map_err(io)?;
    run_command(
        &artifacts_command(&fx, &[("release_dir", path_string(&missing))]),
        &format!("{name} fixture: a release asset is missing"),
        false,
        None,
    )?;
    negatives += 1;

    let swapped = root.join("swapped-archive");
    copy_dir(&fx.release_dir, &swapped).map_err(io)?;
    let archive = swapped.join(format!("{name}-{t0}.tar.gz"));
    let mut bytes = std::fs::read(&archive).map_err(io)?;
    bytes.push(b'\n');
    std::fs::write(&archive, bytes).map_err(io)?;
    run_command(
        &artifacts_command(&fx, &[("release_dir", path_string(&swapped))]),
        &format!("{name} fixture: a public archive differs from the candidate bytes"),
        false,
        None,
    )?;
    negatives += 1;

    let stale = root.join("stale-sidecar");
    std::fs::create_dir_all(&stale).map_err(io)?;
    let stale_sidecar = stale.join("final-candidate-manifest.json.sha256");
    std::fs::write(
        &stale_sidecar,
        format!("{}  final-candidate-manifest.json\n", "0".repeat(64)),
    )
    .map_err(io)?;
    run_command(
        &candidate_local_command(&fx, &[("sidecar", path_string(&stale_sidecar))]),
        &format!("{name} candidate fixture: manifest sidecar does not match"),
        false,
        None,
    )?;
    negatives += 1;

    Ok((positives, negatives))
}
