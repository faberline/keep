//! The promotion-workflow checks, ported from `check_promotion`, `check_app` and
//! `check_not_onboarded` of verify-release-contract.py.

use crate::candidate_contract::{check_candidate, py_list};
use crate::contract::{fail, App, ContractError, Result};
use crate::gke_acceptance_contract::{check_gke_acceptance, check_gke_acceptance_names};
use crate::workflow_text::{
    action_values, all_run_scripts, job_block, job_ids, permission_pairs, with_values,
    workflow_inputs, workflow_triggers,
};
use regex::Regex;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

/// The permissions that the release job of a promotion workflow must declare, exactly.
pub const PROMOTION_PERMISSIONS: [&str; 5] = [
    "actions: read",
    "attestations: read",
    "contents: write",
    "packages: write",
    "pull-requests: read",
];

static SHARED_RELEASE_PIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^faberline/aw/\.github/workflows/service-release\.yml@[0-9a-f]{40}$")
        .expect("valid regex")
});

/// True when `value` is the shared release workflow pinned to a 40-hex commit.
pub fn is_shared_release_pin(value: &str) -> bool {
    SHARED_RELEASE_PIN.is_match(value)
}

/// The value of the shell variable `name` that `scripts/release/apps.sh` defines.
pub fn bash_variable(root: &Path, name: &str) -> Result<String> {
    let apps_sh = root.join("scripts/release/apps.sh");
    let output = match std::process::Command::new("bash")
        .arg("-c")
        .arg("source \"$1\"; printf \"%s\" \"${!2}\"")
        .arg("_")
        .arg(&apps_sh)
        .arg(name)
        .output()
    {
        Ok(output) => output,
        Err(error) => return fail(format!("cannot run bash for {name}: {error}")),
    };
    let value = String::from_utf8_lossy(&output.stdout).to_string();
    let code = output.status.code().unwrap_or(-1);
    if code != 0 || value.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return fail(format!(
            "apps.sh does not define {name}: exit {code}: {}",
            stderr.trim()
        ));
    }
    Ok(value)
}

/// The text of the workflow file `relative` under `root`.
pub fn read_workflow(root: &Path, relative: &Path) -> Result<String> {
    let path = root.join(relative);
    if !path.is_file() {
        return fail(format!("missing file: {}", relative.display()));
    }
    std::fs::read_to_string(&path)
        .map_err(|error| ContractError(format!("cannot read {}: {error}", relative.display())))
}

/// Check the promotion workflow `text` of `app`. Each refusal names the first failed check.
pub fn check_promotion(root: &Path, app: &App, text: &str) -> Result<()> {
    let label = format!("{} promotion", app.name);

    let triggers = workflow_triggers(text)?;
    if triggers != ["workflow_dispatch"] {
        return fail(format!(
            "{label}: the only trigger must be workflow_dispatch, got {}",
            py_list(&triggers)
        ));
    }

    let inputs = workflow_inputs(text)?;
    let expected_inputs: BTreeSet<String> = app.promotion_inputs().into_iter().collect();
    if inputs != expected_inputs {
        return fail(format!(
            "{label}: inputs must be {}, got {}",
            py_list(&expected_inputs),
            py_list(&inputs)
        ));
    }

    let jobs = job_ids(text)?;
    let only_release: BTreeSet<String> = ["release".to_string()].into_iter().collect();
    if jobs != only_release {
        return fail(format!(
            "{label}: the only job must be release, got {}",
            py_list(&jobs)
        ));
    }

    let group = format!(
        "group: {}-release-promotion-{}",
        app.name, "${{ inputs.version }}"
    );
    if !text.contains(&group) {
        return fail(format!(
            "{label}: concurrency group must be {}-release-promotion-<version>",
            app.name
        ));
    }
    if !text.contains("cancel-in-progress: false") {
        return fail(format!("{label}: promotions must not cancel in progress"));
    }

    if !all_run_scripts(text).is_empty() {
        return fail(format!(
            "{label}: the caller must not run a script; the shared workflow does"
        ));
    }
    if text.lines().any(|line| line.trim() == "steps:") {
        return fail(format!("{label}: the caller must not declare steps"));
    }

    let uses = action_values(text);
    if uses.len() != 1 || !is_shared_release_pin(&uses[0]) {
        return fail(format!(
            "{label}: the only uses must be faberline/aw service-release.yml pinned to a 40-hex commit, got {}",
            py_list(&uses)
        ));
    }

    let release = job_block(text, "release")?;
    let pairs = permission_pairs(&release)?;
    let mut sorted_pairs = pairs.clone();
    sorted_pairs.sort();
    let mut expected_pairs: Vec<String> = PROMOTION_PERMISSIONS
        .iter()
        .map(|pair| pair.to_string())
        .collect();
    expected_pairs.sort();
    if sorted_pairs != expected_pairs {
        return fail(format!(
            "{label}: release permissions must be exactly {}, got {}",
            py_list(&expected_pairs),
            py_list(&pairs)
        ));
    }

    let values = with_values(&release)?;
    let dispatched: BTreeSet<String> = app.promotion_inputs().into_iter().collect();
    let mut expected_keys = dispatched.clone();
    expected_keys.insert("app".to_string());
    expected_keys.insert("compatibility_note".to_string());
    let keys: BTreeSet<String> = values.keys().cloned().collect();
    if keys != expected_keys {
        return fail(format!(
            "{label}: with: keys must be {}, got {}",
            py_list(&expected_keys),
            py_list(&keys)
        ));
    }

    let app_value = values.get("app").cloned().unwrap_or_default();
    if app_value != app.name {
        return fail(format!(
            "{label}: with app must be {}, got '{app_value}'",
            app.name
        ));
    }

    for key in &dispatched {
        let value = values.get(key).cloned().unwrap_or_default();
        let passed = format!("${{{{ inputs.{key} }}}}");
        if value != passed {
            return fail(format!(
                "{label}: with {key} must pass inputs.{key} through, got '{value}'"
            ));
        }
    }

    let compatibility = bash_variable(root, "RELEASE_COMPATIBILITY_LINE")?;
    let note_line = format!("compatibility_note: \"{compatibility}\"");
    if !release.lines().any(|line| line.trim() == note_line) {
        return fail(format!(
            "{label}: with compatibility_note must be the apps.sh RELEASE_COMPATIBILITY_LINE as a double-quoted string"
        ));
    }

    Ok(())
}

/// Check one onboarded app: its candidate workflow, then its promotion workflow.
pub fn check_app(root: &Path, app: &App) -> Result<()> {
    if !app.onboarded {
        return fail(format!(
            "{} is not onboarded yet: its release workflows do not exist (flip onboarded when they land)",
            app.name
        ));
    }
    let candidate = read_workflow(root, &app.candidate_workflow())?;
    check_candidate(app, &candidate).map_err(|error| ContractError(error.0))?;
    let promotion = read_workflow(root, &app.promotion_workflow())?;
    check_promotion(root, app, &promotion)?;
    if app.gke_backend == "gke-acceptance" {
        let text = read_workflow(root, Path::new(".github/workflows/gke-acceptance.yml"))?;
        check_gke_acceptance(&text)?;
        check_gke_acceptance_names(&text)?;
    }
    Ok(())
}

/// Refuse when the candidate workflow of an app that is still marked not onboarded exists.
pub fn check_not_onboarded(root: &Path, app: &App) -> Result<()> {
    let path = app.candidate_workflow();
    if root.join(&path).exists() {
        return fail(format!(
            "{} exists but {} is still marked onboarded=False here",
            path.display(),
            app.name
        ));
    }
    Ok(())
}
