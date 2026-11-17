//! The candidate-workflow target checks, ported from `check_candidate_targets` and
//! `check_candidate` of verify-release-contract.py.

use crate::candidate::IMAGE_OWNER;
use crate::contract::App;
use crate::refusal::{refuse, Refusal, Result};
use crate::workflow_text::{
    assert_actions_pinned, job_block, job_ids, parse_needs, workflow_inputs, workflow_triggers,
};
use std::collections::BTreeSet;

/// Map a helper's `contract::Result` to this module's refusal type.
fn lift<T>(result: crate::contract::Result<T>) -> Result<T> {
    result.map_err(|e| Refusal(e.0))
}

/// A Python-style list of strings: `['a', 'b']`.
pub fn py_list<I, S>(items: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let parts: Vec<String> = items
        .into_iter()
        .map(|s| format!("'{}'", s.as_ref()))
        .collect();
    format!("[{}]", parts.join(", "))
}

/// True when every character of `s` passes `ok` and `s` is not empty.
fn all_in(s: &str, ok: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(ok)
}

/// The Python `TRIPLE` pattern: `arch-vendor-system` or `arch-vendor-system-env`.
fn is_triple(name: &str) -> bool {
    let parts: Vec<&str> = name.split('-').collect();
    let (arch, vendor, system, env) = match parts.as_slice() {
        [a, v, s] => (*a, *v, *s, None),
        [a, v, s, e] => (*a, *v, *s, Some(*e)),
        _ => return false,
    };
    all_in(arch, |c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
    }) && matches!(vendor, "unknown" | "apple" | "pc")
        && all_in(system, |c| c.is_ascii_lowercase())
        && env.is_none_or(|e| all_in(e, |c| c.is_ascii_lowercase() || c.is_ascii_digit()))
}

/// The Rust triples named by `<app>-candidate-<triple>-${{ github.run_id }}` references.
pub fn referenced_targets(app: &App, text: &str) -> BTreeSet<String> {
    const SUFFIX: &str = "-${{ github.run_id }}";
    let prefix = format!("{}-candidate-", app.name);
    let mut found = BTreeSet::new();
    let mut pos = 0;
    while let Some(rel) = text[pos..].find(&prefix) {
        let start = pos + rel + prefix.len();
        let matched = text[start..].find(SUFFIX).and_then(|end| {
            let name = &text[start..start + end];
            let valid = all_in(name, |c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            valid.then_some((name, start + end + SUFFIX.len()))
        });
        match matched {
            Some((name, next)) => {
                if is_triple(name) {
                    found.insert(name.to_string());
                }
                pos = next;
            }
            None => pos = pos + rel + 1,
        }
    }
    found
}

/// The target of a build matrix row `- { target: <name>,`, when `line` is one.
fn matrix_target(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("- { target: ")?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!name.is_empty() && rest[name.len()..].starts_with(',')).then_some(name)
}

/// The word lists of every `for target in <words>; do` loop, in text order.
fn for_loops(text: &str) -> Vec<String> {
    const HEAD: &str = "for target in ";
    let mut loops = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(at) = rest.find(HEAD) {
            let after = &rest[at + HEAD.len()..];
            let run = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '-')))
                .unwrap_or(after.len());
            if after[run..].starts_with("; do") {
                loops.push(after[..run].to_string());
            }
            rest = after;
        }
    }
    loops
}

/// Refuse when the build matrix, a job that downloads archives, or a `for target in` loop
/// names targets other than the app's `apps.sh` set.
pub fn check_candidate_targets(app: &App, text: &str) -> Result<()> {
    let label = format!("{} candidate", app.name);
    let expected: BTreeSet<String> = app.targets.iter().cloned().collect();
    let matrix: BTreeSet<String> = text.lines().filter_map(matrix_target).collect();
    if matrix != expected {
        return refuse(format!(
            "{label}: build matrix targets {} differ from apps.sh {}",
            py_list(&matrix),
            py_list(&expected)
        ));
    }
    for job in ["manifest", "verify-candidate", "result"] {
        let seen = referenced_targets(app, &lift(job_block(text, job))?);
        if !seen.is_empty() && seen != expected {
            return refuse(format!(
                "{label}: job {job} downloads targets {}, apps.sh says {}",
                py_list(&seen),
                py_list(&expected)
            ));
        }
    }
    let loops = for_loops(text);
    if loops.is_empty() {
        return refuse(format!(
            "{label}: no `for target in ...; do` loop binds the archives into the manifest"
        ));
    }
    for words in loops {
        let written: Vec<&str> = words.split_whitespace().collect();
        let set: BTreeSet<String> = written.iter().map(|w| w.to_string()).collect();
        if set != expected {
            return refuse(format!(
                "{label}: loop targets {} differ from apps.sh {}",
                py_list(&written),
                py_list(&expected)
            ));
        }
    }
    Ok(())
}

/// Remove the first build matrix row of the app's first target from `text`.
pub fn drop_matrix_row(app: &App, text: &str, label: &str) -> Result<String> {
    let target = app.targets.first().map(String::as_str).unwrap_or("");
    let mut removed = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let is_row = !removed
            && line.ends_with('\n')
            && line
                .trim_start()
                .strip_prefix("- { target: ")
                .and_then(|rest| rest.strip_prefix(target))
                .is_some_and(|rest| rest.starts_with(','));
        if is_row {
            removed = true;
            continue;
        }
        out.push_str(line);
    }
    if !removed {
        return refuse(format!(
            "{label}: control could not find the {target} matrix row"
        ));
    }
    Ok(out)
}

/// Refuse when the candidate workflow does not match the app's contract.
pub fn check_candidate(app: &App, text: &str) -> Result<()> {
    let label = format!("{} candidate", app.name);

    let triggers = lift(workflow_triggers(text))?;
    if triggers.len() != 1 || triggers[0] != "workflow_dispatch" {
        return refuse(format!(
            "{label}: the only trigger must be workflow_dispatch, got {}",
            py_list(&triggers)
        ));
    }

    let inputs = lift(workflow_inputs(text))?;
    let want_inputs: BTreeSet<String> = ["version", "commit"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if inputs != want_inputs {
        return refuse(format!(
            "{label}: inputs must be exactly version and commit, got {}",
            py_list(&inputs)
        ));
    }

    let jobs = lift(job_ids(text))?;
    let want_jobs: BTreeSet<String> = app.candidate_jobs.iter().cloned().collect();
    if jobs != want_jobs {
        return refuse(format!(
            "{label}: jobs must be {}, got {}",
            py_list(&want_jobs),
            py_list(&jobs)
        ));
    }

    lift(assert_actions_pinned(&label, text))?;

    let required = [
        format!(
            "group: {}-release-candidate-${{{{ inputs.version }}}}",
            app.name
        ),
        app.manifest_schema.clone(),
        format!("{IMAGE_OWNER}/{}", app.name),
        format!("refs/tags/{}@", app.name),
        r#"[[ "$GITHUB_REF" == "refs/heads/main" ]]"#.to_string(),
        r#"[[ "$REQUESTED_COMMIT" == "$GITHUB_SHA" ]]"#.to_string(),
        format!(
            ".github/workflows/{}-release-candidate.yml@refs/heads/main",
            app.name
        ),
    ];
    for fragment in &required {
        if !text.contains(fragment.as_str()) {
            return refuse(format!("{label}: required fragment absent: '{fragment}'"));
        }
    }

    let verify = lift(job_block(text, "verify-candidate"))?;
    let verifier = format!("{}/verify-release-candidate.sh", app.scripts_dir);
    if !verify.contains(&verifier) {
        return refuse(format!("{label}: verify-candidate must run {verifier}"));
    }
    if !verify.contains("--mode full") {
        return refuse(format!(
            "{label}: verify-candidate must run the verifier in full mode"
        ));
    }
    if app.shared_scripts && !verify.contains(&format!("--app {}", app.name)) {
        return refuse(format!(
            "{label}: the shared verifier must be told --app {}",
            app.name
        ));
    }

    let result = lift(job_block(text, "result"))?;
    let needs = lift(parse_needs(&result))?;
    let expected_needs: BTreeSet<String> = app
        .candidate_jobs
        .iter()
        .filter(|job| job.as_str() != "result")
        .cloned()
        .collect();
    let needs_set: BTreeSet<String> = needs.iter().cloned().collect();
    if needs.len() != needs_set.len() || needs_set != expected_needs {
        return refuse(format!(
            "{label}: result must need exactly {}, got {}",
            py_list(&expected_needs),
            py_list(&needs)
        ));
    }

    for job in &app.candidate_jobs {
        let bound = if job == "result" {
            result.contains("result:\"success\"") || result.contains("\"result\":\"success\"")
        } else {
            let bare = format!("{job}:\"${{{{ needs.{job}.result }}}}\"");
            let quoted = format!("\"{job}\":\"${{{{ needs.{job}.result }}}}\"");
            result.contains(&bare) || result.contains(&quoted)
        };
        if !bound {
            return refuse(format!(
                "{label}: the final manifest must bind job {job} to its own result"
            ));
        }
    }

    check_candidate_targets(app, text)
}
