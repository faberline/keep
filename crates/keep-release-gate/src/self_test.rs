//! The mutation controls, `check_everything` and `self_test` of verify-release-contract (ported from the Python script).

use crate::candidate_contract::{check_candidate, drop_matrix_row, py_list};
use crate::contract::{apps, fail, find_app, App, ContractError, Result};
use crate::fixture_run::{run_fixture, ARTIFACT_VERIFIER, CANDIDATE_VERIFIER, FIXTURE_APPS};
use crate::gke_acceptance_contract::{check_gke_acceptance, check_gke_acceptance_names};
use crate::promotion_contract::{
    bash_variable, check_app, check_not_onboarded, check_promotion, read_workflow,
};
use crate::skill_contract::{py_repr, replace_once};
use crate::tables_contract::{check_tables, receipt_rows};
use crate::util::sha256_hex;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const APPS_SH: &str = "scripts/release/apps.sh";
const GKE_ACCEPTANCE: &str = ".github/workflows/gke-acceptance.yml";

static PINNED_ACTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(uses: [^@\s]+)@[0-9a-f]{40}").expect("valid regex"));

static PINNED_SHARED_WORKFLOW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(service-release\.yml@)[0-9a-f]{40}").expect("valid regex"));

/// A named mutation: the control name and the action that builds the broken text.
pub type Control<'a> = (String, Box<dyn Fn() -> Result<String> + 'a>);

fn control<'a>(name: String, action: impl Fn() -> Result<String> + 'a) -> Control<'a> {
    (name, Box::new(action))
}

/// Carry a `refusal` error of the candidate and skill checks as a contract error.
fn refusal(result: crate::refusal::Result<String>) -> Result<String> {
    result.map_err(|error| ContractError(error.0))
}

/// Replace every `old` by `new`. Refuses when `old` is absent.
pub fn replace_all(text: &str, old: &str, new: &str, label: &str) -> Result<String> {
    if !text.contains(old) {
        return fail(format!(
            "{label}: mutation anchor is absent: {}",
            py_repr(old)
        ));
    }
    Ok(text.replace(old, new))
}

/// Replace the one `old` by `new`. Refuses unless `old` occurs exactly once.
pub fn replace_once_checked(text: &str, old: &str, new: &str, label: &str) -> Result<String> {
    refusal(replace_once(text, old, new, label))
}

/// Unpin the first action of `text` (`uses: x@<sha>` becomes `uses: x@v4`).
pub fn unpin_first_action(text: &str, label: &str) -> Result<String> {
    if !PINNED_ACTION.is_match(text) {
        return fail(format!("{label}: no pinned action to unpin"));
    }
    Ok(PINNED_ACTION.replacen(text, 1, "${1}@v4").into_owned())
}

/// Unpin the shared release workflow of `text` (`service-release.yml@<sha>` becomes `@main`).
pub fn unpin_shared_workflow(text: &str, label: &str) -> Result<String> {
    if !PINNED_SHARED_WORKFLOW.is_match(text) {
        return fail(format!("{label}: no pinned shared workflow to unpin"));
    }
    Ok(PINNED_SHARED_WORKFLOW
        .replacen(text, 1, "${1}main")
        .into_owned())
}

/// Run `action`, which must refuse. Its refusal is printed and counts as held.
/// An `action` that passes fails the self test.
pub fn expect_static_failure(label: &str, action: impl FnOnce() -> Result<()>) -> Result<()> {
    match action() {
        Err(error) => {
            println!("negative control refused: {label}: {}", error.0);
            Ok(())
        }
        Ok(()) => fail(format!("negative control unexpectedly passed: {label}")),
    }
}

/// The files that the self test must not rewrite, relative to the repository root.
pub fn watched_files() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from(APPS_SH),
        PathBuf::from(CANDIDATE_VERIFIER),
        PathBuf::from(ARTIFACT_VERIFIER),
    ];
    for app in apps() {
        if app.onboarded {
            paths.push(app.candidate_workflow());
            paths.push(app.promotion_workflow());
            if app.gke_backend == "gke-acceptance" {
                paths.push(PathBuf::from(GKE_ACCEPTANCE));
            }
        }
    }
    paths
}

/// The SHA-256 digest of each file of `paths` under `root`.
pub fn digests(root: &Path, paths: &[PathBuf]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|path| {
            let bytes = std::fs::read(root.join(path))
                .map_err(|_| ContractError(format!("missing file: {}", path.display())))?;
            Ok(sha256_hex(&bytes))
        })
        .collect()
}

/// The files of `paths` whose digest differs between `before` and `after`.
pub fn changed_files(paths: &[PathBuf], before: &[String], after: &[String]) -> Vec<String> {
    paths
        .iter()
        .zip(before.iter().zip(after.iter()))
        .filter(|(_, (old, new))| old != new)
        .map(|(path, _)| path.display().to_string())
        .collect()
}

/// Check every app of the table and the shared tables. Returns the summary line.
pub fn check_everything(root: &Path) -> Result<String> {
    let rows = apps();
    let mut onboarded: Vec<String> = Vec::new();
    for app in &rows {
        if app.onboarded {
            check_app(root, app)?;
            onboarded.push(app.name.clone());
        } else {
            check_not_onboarded(root, app)?;
        }
    }
    check_tables(root, &rows, &receipt_rows())?;
    Ok(format!(
        "release contract passed: {} ({} apps), shared tables",
        onboarded.join(" "),
        onboarded.len()
    ))
}

/// The twelve mutations of the candidate workflow of `app`.
pub fn candidate_controls<'a>(app: &'a App, text: &'a str) -> Vec<Control<'a>> {
    let label = format!("{} candidate", app.name);
    let name = |suffix: &str| format!("{label}: {suffix}");

    let mut schema_bumped = app.manifest_schema.clone();
    schema_bumped.pop();
    schema_bumped.push('9');

    let stale_archive = format!(
        "{}-candidate-{}-",
        app.name,
        app.targets.last().cloned().unwrap_or_default()
    );
    let loop_all = format!("for target in {}; do", app.targets.join(" "));
    let loop_dropped = format!(
        "for target in {}; do",
        app.targets[..app.targets.len().saturating_sub(1)].join(" ")
    );
    let schema = app.manifest_schema.clone();

    vec![
        control(name("commit input renamed"), {
            let l = label.clone();
            move || replace_once_checked(text, "      commit:\n", "      commit_sha:\n", &l)
        }),
        control(name("result job renamed"), {
            let l = label.clone();
            move || replace_once_checked(text, "\n  result:\n", "\n  results:\n", &l)
        }),
        control(name("action unpinned"), {
            let l = label.clone();
            move || unpin_first_action(text, &l)
        }),
        control(name("manifest schema bumped silently"), {
            let l = label.clone();
            move || replace_all(text, &schema, &schema_bumped, &l)
        }),
        control(name("verifier downgraded to local mode"), {
            let l = label.clone();
            move || replace_all(text, "--mode full", "--mode local", &l)
        }),
        control(name("commit identity proof dropped"), {
            let l = label.clone();
            move || {
                replace_all(
                    text,
                    r#"[[ "$REQUESTED_COMMIT" == "$GITHUB_SHA" ]]"#,
                    "true",
                    &l,
                )
            }
        }),
        control(name("build result binding faked"), {
            let l = label.clone();
            move || {
                replace_all(
                    text,
                    r#"build:"${{ needs.build.result }}""#,
                    r#"build:"success""#,
                    &l,
                )
            }
        }),
        control(name("main-only guard dropped"), {
            let l = label.clone();
            move || replace_all(text, "refs/heads/main", "refs/heads/release", &l)
        }),
        control(name("extra trigger added"), {
            let l = label.clone();
            move || {
                replace_once_checked(
                    text,
                    "on:\n  workflow_dispatch:\n",
                    "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n",
                    &l,
                )
            }
        }),
        control(name("stale target archive still downloaded"), {
            let l = label.clone();
            let freebsd = format!("{}-candidate-x86_64-unknown-freebsd-", app.name);
            move || replace_all(text, &stale_archive, &freebsd, &l)
        }),
        control(name("manifest loop drops a target"), {
            let l = label.clone();
            move || replace_all(text, &loop_all, &loop_dropped, &l)
        }),
        control(name("build matrix row dropped"), {
            let l = label.clone();
            move || refusal(drop_matrix_row(app, text, &l))
        }),
    ]
}

/// The mutations of the promotion workflow of `app`, in the order of the Python script.
pub fn promotion_controls<'a>(
    root: &Path,
    app: &'a App,
    text: &'a str,
) -> Result<Vec<Control<'a>>> {
    let label = format!("{} promotion", app.name);
    let name = |suffix: &str| format!("{label}: {suffix}");
    let compatibility = bash_variable(root, "RELEASE_COMPATIBILITY_LINE")?;
    let tag_trigger = format!(
        "on:\n  push:\n    tags: ['{}@*']\n  workflow_dispatch:\n",
        app.name
    );

    Ok(vec![
        control(name("tag push trigger added"), {
            let l = label.clone();
            move || replace_once_checked(text, "on:\n  workflow_dispatch:\n", &tag_trigger, &l)
        }),
        control(name("candidate run id input dropped"), {
            let l = label.clone();
            move || {
                replace_once_checked(
                    text,
                    "      candidate_run_id:\n",
                    "      candidate_run:\n",
                    &l,
                )
            }
        }),
        control(name("release job renamed"), {
            let l = label.clone();
            move || replace_once_checked(text, "\n  release:\n", "\n  publish:\n", &l)
        }),
        control(name("shared workflow unpinned"), {
            let l = label.clone();
            move || unpin_shared_workflow(text, &l)
        }),
        control(name("shared workflow from another path"), {
            let l = label.clone();
            move || replace_once_checked(text, "service-release.yml@", "other-release.yml@", &l)
        }),
        control(name("inline step added"), {
            let l = label.clone();
            move || {
                replace_once_checked(
                    text,
                    "    with:\n      app: keep\n",
                    "    steps:\n      - run: echo hi\n    with:\n      app: keep\n",
                    &l,
                )
            }
        }),
        control(name("extra permission"), {
            let l = label.clone();
            move || {
                replace_once_checked(
                    text,
                    "      pull-requests: read\n",
                    "      pull-requests: read\n      id-token: write\n",
                    &l,
                )
            }
        }),
        control(name("app input changed"), {
            let l = label.clone();
            move || replace_once_checked(text, "      app: keep\n", "      app: relay\n", &l)
        }),
        control(name("dispatch input not passed through"), {
            let l = label.clone();
            move || {
                replace_once_checked(
                    text,
                    "      version: ${{ inputs.version }}\n",
                    "      version: \"0.0.0\"\n",
                    &l,
                )
            }
        }),
        control(name("compatibility line reworded"), {
            let l = label.clone();
            move || replace_once_checked(text, &compatibility, "- Compatibility: unchanged.", &l)
        }),
        control(name("verify script overridden"), {
            let l = label.clone();
            move || {
                replace_once_checked(
                    text,
                    "      app: keep\n",
                    "      app: keep\n      verify_script: scripts/other.sh\n",
                    &l,
                )
            }
        }),
    ])
}

/// Run every positive and negative check of the contract against `root`.
/// Returns the summary line, or the first refusal.
pub fn self_test(root: &Path) -> Result<String> {
    let paths = watched_files();
    let before = digests(root, &paths)?;
    let rows = apps();
    let mut positives: usize = 0;
    let mut negatives: usize = 0;

    let onboarded: Vec<&App> = rows.iter().filter(|app| app.onboarded).collect();
    for &app in &onboarded {
        check_app(root, app)?;
        positives += 1;
    }
    check_tables(root, &rows, &receipt_rows())?;
    positives += 1;
    for app in rows.iter().filter(|app| !app.onboarded) {
        check_not_onboarded(root, app)?;
    }

    for &app in &onboarded {
        let candidate = read_workflow(root, &app.candidate_workflow())?;
        let promotion = read_workflow(root, &app.promotion_workflow())?;
        for (name, mutate) in candidate_controls(app, &candidate) {
            expect_static_failure(&name, || {
                let mutated = mutate()?;
                check_candidate(app, &mutated).map_err(|error| ContractError(error.0))
            })?;
            negatives += 1;
        }
        for (name, mutate) in promotion_controls(root, app, &promotion)? {
            expect_static_failure(&name, || {
                let mutated = mutate()?;
                check_promotion(root, app, &mutated)
            })?;
            negatives += 1;
        }
    }

    let acceptance = read_workflow(root, Path::new(GKE_ACCEPTANCE))?;
    check_gke_acceptance(&acceptance)?;
    check_gke_acceptance_names(&acceptance)?;
    positives += 1;
    for (label, old, new, names_only) in [
        (
            "gke-acceptance: extra trigger",
            "on:\n  workflow_dispatch:",
            "on:\n  push:\n  workflow_dispatch:",
            false,
        ),
        (
            "gke-acceptance: job renamed",
            "name: deploy + verify on GKE",
            "name: deploy and verify on GKE",
            true,
        ),
        (
            "gke-acceptance: harness step renamed",
            "- name: Run acceptance harness",
            "- name: Run harness",
            true,
        ),
        (
            "gke-acceptance: park step renamed",
            "- name: Park node pool (belt and suspenders)",
            "- name: Park pool",
            true,
        ),
        (
            "gke-acceptance: verdict changed",
            "[keep] PASS: readyz + round-trip + durability",
            "[keep] PASS: readyz",
            true,
        ),
    ] {
        let mutated = replace_once_checked(&acceptance, old, new, label)?;
        if names_only {
            expect_static_failure(label, || check_gke_acceptance_names(&mutated))?;
        } else {
            expect_static_failure(label, || check_gke_acceptance(&mutated))?;
        }
        negatives += 1;
    }

    expect_static_failure("table: a non-onboarded app is checked as onboarded", || {
        let mut clone = find_app("keep")?;
        clone.onboarded = false;
        check_app(root, &clone)
    })?;
    negatives += 1;

    let mut fixture_names: Vec<&str> = Vec::new();
    for name in FIXTURE_APPS {
        let (pos, neg) = run_fixture(&find_app(name)?)?;
        positives += pos;
        negatives += neg;
        fixture_names.push(name);
    }

    let after = digests(root, &paths)?;
    if before != after {
        let changed = changed_files(&paths, &before, &after);
        return fail(format!(
            "self-test must not rewrite checked files: {}",
            py_list(changed)
        ));
    }
    Ok(format!(
        "release contract self-test passed: {positives} positive checks, \
         {negatives} negative controls refused, fixtures {}",
        fixture_names.join(" ")
    ))
}
