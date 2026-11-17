//! The gke-acceptance workflow checks, ported from `check_gke_acceptance` and
//! `check_gke_acceptance_names` of verify-release-contract.py.

use crate::candidate_contract::py_list;
use crate::contract::{fail, Result};
use crate::receipt::{ACCEPTANCE_JOB, HARNESS_STEP, PARK_STEP, VERIFY_IMAGE_STEP};
use crate::workflow_text::{
    job_block, job_ids, run_script, step_block, workflow_inputs, workflow_triggers,
};
use regex::Regex;
use std::collections::BTreeSet;
use std::sync::LazyLock;

/// The apps that the gke-acceptance workflow deploys and verifies.
pub const GKE_HARNESS_APPS: [&str; 1] = ["keep"];

const LABEL: &str = "gke-acceptance";
const PREPARE_EVIDENCE_STEP: &str = "Prepare evidence directory";

static RUN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^run-name: .*inputs\.image != ''.*image=\{0\}").expect("valid regex")
});

/// The `<APP>_IMAGE` env line of the acceptance job for `app`.
pub fn env_line(app: &str) -> String {
    let upper = app.to_uppercase();
    format!(
        "{upper}_IMAGE: ${{{{ contains(inputs.apps, '{app}') && inputs.image || needs.build-{app}.outputs.image }}}}"
    )
}

/// The `if:` line that skips the build job of `app` when a prebuilt image is given.
pub fn build_gate(app: &str) -> String {
    format!("if: contains(inputs.apps, '{app}') && inputs.image == ''")
}

/// Check the gke-acceptance workflow `text`. Each refusal names the first failed check.
pub fn check_gke_acceptance(text: &str) -> Result<()> {
    let triggers = workflow_triggers(text)?;
    if triggers != ["workflow_dispatch"] {
        return fail(format!(
            "{LABEL}: the only trigger must be workflow_dispatch, got {}",
            py_list(&triggers)
        ));
    }
    let inputs = workflow_inputs(text)?;
    let expected_inputs: BTreeSet<String> = ["apps", "ref", "image", "topology"]
        .into_iter()
        .map(String::from)
        .collect();
    if inputs != expected_inputs {
        return fail(format!(
            "{LABEL}: inputs must be apps, ref, image, topology, got {}",
            py_list(&inputs)
        ));
    }
    if !RUN_NAME.is_match(text) {
        return fail(format!(
            "{LABEL}: run-name must carry the prebuilt image so release.sh can tell the runs apart"
        ));
    }
    let expected_jobs: BTreeSet<String> = GKE_HARNESS_APPS
        .iter()
        .map(|app| format!("build-{app}"))
        .chain(std::iter::once("acceptance".to_string()))
        .collect();
    let jobs = job_ids(text)?;
    if jobs != expected_jobs {
        return fail(format!(
            "{LABEL}: jobs must be {}, got {}",
            py_list(&expected_jobs),
            py_list(&jobs)
        ));
    }
    for app in GKE_HARNESS_APPS {
        if !job_block(text, &format!("build-{app}"))?.contains(&build_gate(app)) {
            return fail(format!(
                "{LABEL}: build-{app} must be skipped when a prebuilt image is given"
            ));
        }
    }

    let acceptance = job_block(text, "acceptance")?;
    if !acceptance.contains(&format!("name: {ACCEPTANCE_JOB}")) {
        return fail(format!(
            "{LABEL}: the acceptance job must be named '{ACCEPTANCE_JOB}'"
        ));
    }
    for app in GKE_HARNESS_APPS {
        if !acceptance.contains(&env_line(app)) {
            return fail(format!(
                "{LABEL}: {}_IMAGE must prefer the prebuilt image for a selected app",
                app.to_uppercase()
            ));
        }
    }

    let verify = step_block(&acceptance, VERIFY_IMAGE_STEP)?;
    if !verify.contains("if: inputs.image != ''") {
        return fail(format!(
            "{LABEL}: '{VERIFY_IMAGE_STEP}' must run whenever a prebuilt image is given"
        ));
    }
    step_block(&acceptance, PREPARE_EVIDENCE_STEP)?;
    let verify_at = acceptance
        .find(&format!("- name: {VERIFY_IMAGE_STEP}"))
        .unwrap_or(usize::MAX);
    let prepare_at = acceptance
        .find(&format!("- name: {PREPARE_EVIDENCE_STEP}"))
        .unwrap_or(0);
    if verify_at > prepare_at {
        return fail(format!(
            "{LABEL}: '{VERIFY_IMAGE_STEP}' must run before any cluster work"
        ));
    }

    let script = run_script(&verify)?;
    for required in [
        "[ \"${#apps[@]}\" -eq 1 ]",
        "@sha256:[0-9a-f]{64}$",
        "${OWNER}",
        "${app}",
    ] {
        if !script.contains(required) {
            return fail(format!(
                "{LABEL}: '{VERIFY_IMAGE_STEP}' must check '{required}'"
            ));
        }
    }
    if !verify.contains("OWNER: ${{ github.repository_owner }}") {
        return fail(format!(
            "{LABEL}: the image owner must come from github.repository_owner"
        ));
    }
    for step in [HARNESS_STEP, PARK_STEP] {
        step_block(&acceptance, step)?;
    }
    Ok(())
}

/// Check the names that the receipt maker reads from the gke-acceptance workflow `text`.
pub fn check_gke_acceptance_names(text: &str) -> Result<()> {
    let acceptance = job_block(text, "acceptance")?;
    if !acceptance.contains(&format!("name: {ACCEPTANCE_JOB}")) {
        return fail(format!(
            "{LABEL}: the acceptance job must be named '{ACCEPTANCE_JOB}' for make-gke-release-receipt.py"
        ));
    }
    for step in [VERIFY_IMAGE_STEP, HARNESS_STEP, PARK_STEP] {
        step_block(&acceptance, step)?;
    }
    if !text.contains("[keep] PASS: readyz + round-trip + durability") {
        return fail(format!(
            "{LABEL}: the verdict line must be '[keep] PASS: readyz + round-trip + durability'"
        ));
    }
    Ok(())
}
