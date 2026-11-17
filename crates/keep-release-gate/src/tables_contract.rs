//! The table checks of verify-release-contract, ported from `bash_values`,
//! `bash_predicate`, `check_constants`, `check_receipt_rows` and `check_tables`
//! of verify-release-contract.py. `bash_variable` is in `promotion_contract`.

use crate::candidate;
use crate::candidate_contract::py_list;
use crate::contract::{fail, App, Result};
use crate::receipt;
use std::collections::BTreeMap;
use std::path::Path;

/// One row of the receipt table: what the Rust receipt code writes for an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptRow {
    pub targets: Vec<String>,
    pub backend: String,
    pub functional: Vec<String>,
}

/// Run the function `function` of `apps.sh` with `app`. Returns the exit code, stdout and trimmed stderr.
fn run_apps_function(root: &Path, function: &str, app: &str) -> Result<(i32, String, String)> {
    let apps_sh = root.join("scripts/release/apps.sh");
    let output = match std::process::Command::new("bash")
        .args(["-c", "source \"$1\"; shift; \"$@\"", "_"])
        .arg(&apps_sh)
        .arg(function)
        .arg(app)
        .output()
    {
        Ok(output) => output,
        Err(error) => return fail(format!("cannot run bash for {function}: {error}")),
    };
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Ok((code, stdout, stderr))
}

/// The non-empty lines that `apps.sh` function `function` prints for `app`.
pub fn bash_values(root: &Path, function: &str, app: &str) -> Result<Vec<String>> {
    let (code, stdout, stderr) = run_apps_function(root, function, app)?;
    if code != 0 {
        return fail(format!("apps.sh {function} {app}: exit {code}: {stderr}"));
    }
    Ok(stdout
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// True when `apps.sh` function `function` returns 0 for `app`. Exit 1 is false; any other exit code is refused.
pub fn bash_predicate(root: &Path, function: &str, app: &str) -> Result<bool> {
    let (code, _stdout, stderr) = run_apps_function(root, function, app)?;
    if code != 0 && code != 1 {
        return fail(format!("apps.sh {function} {app}: exit {code}: {stderr}"));
    }
    Ok(code == 0)
}

/// The receipt table of the Rust receipt code. Only `keep` is known.
pub fn receipt_rows() -> BTreeMap<String, ReceiptRow> {
    let mut rows = BTreeMap::new();
    rows.insert(
        "keep".to_string(),
        ReceiptRow {
            targets: candidate::app_targets("keep")
                .expect("keep is a known app")
                .iter()
                .map(|target| target.to_string())
                .collect(),
            backend: "gke-acceptance".to_string(),
            functional: receipt::HARNESS_FIELDS
                .iter()
                .map(|field| field.to_string())
                .collect(),
        },
    );
    rows
}

/// Refuse the first triple `(attribute, actual, expected)` whose two texts differ.
pub fn check_constants(pairs: &[(&str, &str, &str)]) -> Result<()> {
    for (attribute, actual, expected) in pairs {
        if actual != expected {
            return fail(format!(
                "receipt maker {attribute} = '{actual}', expected '{expected}'"
            ));
        }
    }
    Ok(())
}

/// Compare the receipt table `rows` with the Rust code and the App table `apps`.
pub fn check_receipt_rows(apps: &[App], rows: &BTreeMap<String, ReceiptRow>) -> Result<()> {
    let mut shared: Vec<&App> = apps.iter().filter(|app| app.shared_scripts).collect();
    shared.sort_by(|a, b| a.name.cmp(&b.name));
    let shared_names: Vec<&str> = shared.iter().map(|app| app.name.as_str()).collect();
    let row_names: Vec<&str> = rows.keys().map(String::as_str).collect();
    if row_names != shared_names {
        return fail(format!(
            "receipt maker knows {}, shared-script apps are {}",
            py_list(&row_names),
            py_list(&shared_names)
        ));
    }

    // The two sorted lists have the same names, so each app meets its own row.
    for (app, (_, row)) in shared.into_iter().zip(rows) {
        let name = app.name.as_str();
        if row.targets != app.targets {
            return fail(format!(
                "receipt maker targets for {name} differ from this table"
            ));
        }
        if row.backend != app.gke_backend {
            return fail(format!(
                "receipt maker backend for {name} is {}, table says {}",
                row.backend, app.gke_backend
            ));
        }
        if row.functional != app.functional {
            return fail(format!(
                "receipt maker functional fields for {name} differ from this table"
            ));
        }
        if candidate::candidate_schema(name) != app.manifest_schema {
            return fail(format!(
                "receipt maker candidate schema for {name} differs from this table"
            ));
        }
        if candidate::receipt_schema(name) != app.receipt_schema {
            return fail(format!(
                "receipt maker receipt schema for {name} differs from this table"
            ));
        }
        let job_keys: Vec<String> = candidate::final_jobs(name).keys().cloned().collect();
        if job_keys != app.candidate_jobs {
            return fail(format!(
                "receipt maker job inventory for {name} differs from this table"
            ));
        }
    }

    check_constants(&[
        ("REPOSITORY", candidate::REPOSITORY, "faberline/keep"),
        ("IMAGE_OWNER", candidate::IMAGE_OWNER, "ghcr.io/faberline"),
        (
            "ACCEPTANCE_JOB",
            receipt::ACCEPTANCE_JOB,
            "deploy + verify on GKE",
        ),
        (
            "VERIFY_IMAGE_STEP",
            receipt::VERIFY_IMAGE_STEP,
            "Verify prebuilt image input",
        ),
        (
            "HARNESS_STEP",
            receipt::HARNESS_STEP,
            "Run acceptance harness",
        ),
        (
            "PARK_STEP",
            receipt::PARK_STEP,
            "Park node pool (belt and suspenders)",
        ),
    ])
}

/// Compare `apps.sh` under `root` with the App table `apps` and the receipt table `rows`.
pub fn check_tables(root: &Path, apps: &[App], rows: &BTreeMap<String, ReceiptRow>) -> Result<()> {
    for app in apps {
        let name = app.name.as_str();
        let same = |function: &str, expected: &[String]| -> Result<()> {
            let actual = bash_values(root, function, name)?;
            if actual != expected {
                return fail(format!(
                    "apps.sh {function} {name} = {}, this table says {}",
                    py_list(&actual),
                    py_list(expected)
                ));
            }
            Ok(())
        };
        same("release_app_root", std::slice::from_ref(&app.root))?;
        same(
            "release_app_scripts_dir",
            std::slice::from_ref(&app.scripts_dir),
        )?;
        same("release_app_targets", &app.targets)?;
        same(
            "release_app_gke_backend",
            std::slice::from_ref(&app.gke_backend),
        )?;
        same(
            "release_app_receipt_name",
            std::slice::from_ref(&app.receipt_name),
        )?;
        same(
            "release_app_receipt_schema",
            std::slice::from_ref(&app.receipt_schema),
        )?;
        same(
            "release_app_promotion_input_prefix",
            std::slice::from_ref(&app.promotion_prefix),
        )?;
        same(
            "release_app_manifest_schema",
            std::slice::from_ref(&app.manifest_schema),
        )?;
        same("release_app_candidate_jobs", &app.candidate_jobs)?;
        if bash_predicate(root, "release_app_uses_shared_scripts", name)? != app.shared_scripts {
            return fail(format!(
                "apps.sh disagrees on whether {name} uses the shared scripts"
            ));
        }
        if bash_predicate(root, "release_app_promotion_takes_attempt", name)? != app.takes_attempt {
            return fail(format!(
                "apps.sh disagrees on whether {name} promotion takes a run attempt"
            ));
        }
        if app.shared_scripts {
            same("release_app_functional_fields", &app.functional)?;
        }
    }
    if bash_predicate(root, "release_app_known", "loom")? {
        return fail("apps.sh must not know loom: it has no candidate or promotion");
    }
    check_receipt_rows(apps, rows)
}
