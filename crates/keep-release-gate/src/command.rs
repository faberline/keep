//! The `release-gate` command: parse the arguments, run the checks and return the exit code and the text for stdout and stderr.

use crate::contract::find_app;
use crate::promotion_contract::check_app;
use crate::receipt::{make_receipt, ReceiptInputs};
use crate::self_test::{check_everything, self_test};
use crate::tables_contract::{check_tables, receipt_rows};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};

/// The usage line of the command.
pub const USAGE: &str = "usage: release-gate receipt --app keep --backend gke-acceptance --candidate-manifest <file> --output <file> --gh-run <file> --evidence-dir <dir>";

/// The usage line of `verify-release-contract`.
pub const VERIFY_USAGE: &str =
    "usage: release-gate verify-release-contract [--app keep] [--self-test] [--static-only]";

/// The options of `receipt`. Each one takes a value.
const OPTIONS: [&str; 10] = [
    "app",
    "backend",
    "candidate-manifest",
    "output",
    "run",
    "images",
    "acceptance",
    "cleanup",
    "gh-run",
    "evidence-dir",
];

/// The options of `receipt` that must be given.
const REQUIRED: [&str; 4] = ["app", "backend", "candidate-manifest", "output"];

/// Run the command with `args`, the words after the program name. Returns the exit code, the text for stdout and the text for stderr.
pub fn run(args: &[String]) -> (i32, String, String) {
    match args.first().map(String::as_str) {
        None => (2, String::new(), format!("{USAGE}\n")),
        Some("receipt") => receipt(&args[1..]),
        Some("verify-release-contract") => verify_release_contract(&args[1..], Path::new(".")),
        Some(other) => (
            2,
            String::new(),
            format!("release-gate: unknown subcommand {other}\n"),
        ),
    }
}

/// A usage error of `verify-release-contract`: exit 2 with `message` on stderr.
fn verify_usage_error(message: &str) -> (i32, String, String) {
    (
        2,
        String::new(),
        format!("release-gate verify-release-contract: {message}\n"),
    )
}

/// The text of a finished `verify-release-contract` run: the summary on stdout, or the refusal on stderr with exit 1.
fn verify_finished<E: Display>(result: Result<String, E>) -> (i32, String, String) {
    match result {
        Ok(summary) => (0, format!("{summary}\n"), String::new()),
        Err(error) => (
            1,
            String::new(),
            format!("verify-release-contract: refused: {error}\n"),
        ),
    }
}

/// The `verify-release-contract` subcommand. `args` are the words after the command name; `root` is the repository root.
pub fn verify_release_contract(args: &[String], root: &Path) -> (i32, String, String) {
    let mut app: Option<String> = None;
    let mut static_only = false;
    let mut self_test_only = false;
    let mut items = args.iter();
    while let Some(item) = items.next() {
        match item.as_str() {
            "--static-only" => static_only = true,
            "--self-test" => self_test_only = true,
            "--app" => match items.next() {
                Some(value) => app = Some(value.clone()),
                None => return verify_usage_error("--app needs a value"),
            },
            other => return verify_usage_error(&format!("unknown option {other}")),
        }
    }
    if self_test_only {
        if app.is_some() || static_only {
            return verify_usage_error("--self-test covers every onboarded app; drop --app");
        }
        return verify_finished(self_test(root));
    }
    let app = match app {
        Some(app) => app,
        None if static_only => "keep".to_string(),
        None => return verify_finished(check_everything(root)),
    };
    let Ok(row) = find_app(&app) else {
        return verify_usage_error(&format!("invalid choice: {app}"));
    };
    let result = check_app(root, &row)
        .and_then(|()| check_tables(root, std::slice::from_ref(&row), &receipt_rows()))
        .map(|()| format!("release contract passed: {}", row.name));
    verify_finished(result)
}

/// A usage error of `receipt`: exit 2 with `message` on stderr.
fn usage_error(message: &str) -> (i32, String, String) {
    (
        2,
        String::new(),
        format!("release-gate receipt: {message}\n"),
    )
}

/// The `receipt` subcommand. `args` are the words after the word `receipt`.
fn receipt(args: &[String]) -> (i32, String, String) {
    let mut values: BTreeMap<&str, String> = BTreeMap::new();
    let mut items = args.iter();
    while let Some(item) = items.next() {
        let name = match item.strip_prefix("--") {
            Some(name) if OPTIONS.contains(&name) => name,
            _ => return usage_error(&format!("unknown option {item}")),
        };
        let Some(value) = items.next() else {
            return usage_error(&format!("--{name} needs a value"));
        };
        values.insert(name, value.clone());
    }

    if let Some(missing) = REQUIRED.iter().find(|name| !values.contains_key(*name)) {
        return usage_error(&format!("--{missing} is required"));
    }
    let get = |name: &str| values.get(name).map_or("", String::as_str);
    let path = |name: &str| values.get(name).map(|value| PathBuf::from(value.as_str()));

    let app = get("app");
    if app != "keep" {
        return usage_error(&format!("invalid choice: {app}"));
    }
    let backend = get("backend");
    if backend != "gcp" && backend != "gke-acceptance" {
        return usage_error(&format!("invalid choice: {backend}"));
    }

    let run_path = path("run");
    let images_path = path("images");
    let acceptance_path = path("acceptance");
    let cleanup_path = path("cleanup");
    let gh_run_path = path("gh-run");
    let evidence_path = path("evidence-dir");
    let inputs = ReceiptInputs {
        run: run_path.as_deref(),
        images: images_path.as_deref(),
        acceptance: acceptance_path.as_deref(),
        cleanup: cleanup_path.as_deref(),
        gh_run: gh_run_path.as_deref(),
        evidence_dir: evidence_path.as_deref(),
    };
    let output = get("output");
    match make_receipt(
        app,
        backend,
        Path::new(get("candidate-manifest")),
        Path::new(output),
        &inputs,
    ) {
        Ok(_) => (0, format!("GKE receipt written: {output}\n"), String::new()),
        Err(refusal) => (1, String::new(), format!("{refusal}\n")),
    }
}
