//! Acceptance and refusal tests for `validate_gke_acceptance`. Each refusal asserts the exact text.

use keep_release_gate::candidate::{validate_candidate, CandidateSummary, IMAGE_OWNER};
use keep_release_gate::fixtures::{
    candidate, gke_acceptance, write_harness_evidence, FIXTURE_COMMIT,
};
use keep_release_gate::receipt::{
    validate_gke_acceptance, ACCEPTANCE_JOB, HARNESS_STEP, PARK_STEP, VERIFY_IMAGE_STEP,
};
use keep_release_gate::util::{is_lower_hex, sha256_hex};
use serde_json::{json, Value};
use std::path::Path;

const P: &str = "GKE receipt refused: ";

/// A mutation of the run record, and the refusal text it must produce.
type RunCase = (Box<dyn FnOnce(&mut Value)>, String);
/// A change of the evidence directory, and the refusal text it must produce.
type EvidenceCase = (Box<dyn FnOnce(&Path)>, String);
/// A removed evidence file, and the start of its refusal text (the path that follows is not fixed).
type MissingCase = (Box<dyn FnOnce(&Path)>, &'static str);

/// The checked candidate of the fixture keep release.
fn summary() -> CandidateSummary {
    let value = candidate("keep", "0.4.13", FIXTURE_COMMIT, "12345", "1");
    let bytes = serde_json::to_vec(&value).unwrap();
    validate_candidate("keep", &bytes, value.as_object().unwrap()).unwrap()
}

/// The image that the candidate deploys.
fn image(s: &CandidateSummary) -> String {
    format!("{IMAGE_OWNER}/keep@{}", s.root_digest)
}

/// The fixture gke-acceptance run record for the candidate.
fn good_run(s: &CandidateSummary) -> Value {
    gke_acceptance("keep", FIXTURE_COMMIT, &image(s))
}

/// A temporary evidence directory that holds the fixture harness evidence.
fn evidence(s: &CandidateSummary) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_harness_evidence("keep", dir.path(), &image(s)).unwrap();
    dir
}

/// The refusal text of the fixture run after `mutate`.
fn refused_run(mutate: impl FnOnce(&mut Value)) -> String {
    let s = summary();
    let mut run = good_run(&s);
    mutate(&mut run);
    let dir = evidence(&s);
    validate_gke_acceptance("keep", &s, run.as_object().unwrap(), dir.path())
        .unwrap_err()
        .to_string()
}

/// The refusal text of the fixture run with the evidence directory changed by `mutate`.
fn refused_evidence(mutate: impl FnOnce(&Path)) -> String {
    let s = summary();
    let run = good_run(&s);
    let dir = evidence(&s);
    mutate(dir.path());
    validate_gke_acceptance("keep", &s, run.as_object().unwrap(), dir.path())
        .unwrap_err()
        .to_string()
}

/// The acceptance job of a run record.
fn acceptance_job(run: &mut Value) -> &mut Value {
    run["jobs"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|job| job["name"] == ACCEPTANCE_JOB)
        .unwrap()
}

/// The step `name` of the acceptance job.
fn step<'a>(run: &'a mut Value, name: &str) -> &'a mut Value {
    acceptance_job(run)["steps"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["name"] == name)
        .unwrap()
}

/// Write `bytes` to `dir/rel`.
fn put(dir: &Path, rel: &str, bytes: impl AsRef<[u8]>) {
    std::fs::write(dir.join(rel), bytes).unwrap();
}

#[test]
fn fixture_run_is_accepted() {
    let s = summary();
    let dir = evidence(&s);
    let accepted =
        validate_gke_acceptance("keep", &s, good_run(&s).as_object().unwrap(), dir.path()).unwrap();
    assert_eq!(accepted.run_id, "gha-99001-1");
    assert_eq!(
        accepted.run_url,
        "https://github.com/faberline/keep/actions/runs/99001"
    );
    assert_eq!(accepted.evidence.len(), 3);
    for key in ["manifests_sha256", "results_sha256", "verdict_sha256"] {
        assert!(is_lower_hex(&accepted.evidence[key], 64), "{key}");
    }
    assert_eq!(
        accepted.evidence["results_sha256"],
        sha256_hex(b"keep PASS\n")
    );
    assert_eq!(
        accepted.evidence["verdict_sha256"],
        sha256_hex(b"[keep] PASS: readyz + round-trip + durability\n")
    );
}

#[test]
fn missing_run_fields_are_refused() {
    let msg = refused_run(|run: &mut Value| {
        run.as_object_mut().unwrap().remove("jobs");
    });
    assert_eq!(
        msg,
        format!(
            "{P}gh run record is missing fields; use --json databaseId,attempt,conclusion,status,url,headSha,event,workflowName,displayTitle,jobs"
        )
    );
}

#[test]
fn bad_run_identity_is_refused() {
    let cases: Vec<(&str, Value)> = vec![
        ("databaseId", json!(0)),
        ("databaseId", json!("99001")),
        ("attempt", json!(1.5)),
        ("attempt", json!(true)),
    ];
    for (key, value) in cases {
        let msg = refused_run(|run: &mut Value| run[key] = value.clone());
        assert_eq!(
            msg,
            format!("{P}gh run identity is invalid"),
            "{key}={value}"
        );
    }
}

#[test]
fn gh_run_mutations_are_refused() {
    let commit = FIXTURE_COMMIT;
    let cases: Vec<RunCase> = vec![
        (
            Box::new(|run: &mut Value| run["conclusion"] = json!("failure")),
            "run did not conclude success".into(),
        ),
        (
            Box::new(|run: &mut Value| run["status"] = json!("in_progress")),
            "run did not conclude success".into(),
        ),
        (
            Box::new(|run: &mut Value| run["headSha"] = json!("f".repeat(40))),
            "run head commit is not the candidate commit".into(),
        ),
        (
            Box::new(move |run: &mut Value| {
                run["displayTitle"] = json!(format!("gke-acceptance keep @ {commit}"))
            }),
            "run was not dispatched for this app with the candidate image".into(),
        ),
        (
            Box::new(move |run: &mut Value| {
                run["displayTitle"] = json!(format!("gke-acceptance relay @ {commit} image=x"))
            }),
            "run was not dispatched for this app with the candidate image".into(),
        ),
        (
            Box::new(|run: &mut Value| run["workflowName"] = json!("keep-test-image")),
            "run is not a gke-acceptance workflow run".into(),
        ),
        (
            Box::new(|run: &mut Value| run["event"] = json!("push")),
            "run was not dispatched by hand".into(),
        ),
        (
            Box::new(|run: &mut Value| {
                run["url"] = json!("https://github.com/someone/else/actions/runs/99001")
            }),
            "run URL does not belong to this repository and run".into(),
        ),
        (
            Box::new(|run: &mut Value| run["jobs"] = json!([])),
            "run has no job inventory".into(),
        ),
        (
            Box::new(|run: &mut Value| run["jobs"] = json!("x")),
            "run has no job inventory".into(),
        ),
        (
            Box::new(|run: &mut Value| run["jobs"][0]["conclusion"] = json!("success")),
            "a build job ran; the candidate image must be the only image in play".into(),
        ),
        (
            Box::new(|run: &mut Value| run["jobs"][0] = json!("x")),
            "a build job ran; the candidate image must be the only image in play".into(),
        ),
        (
            Box::new(|run: &mut Value| step(run, PARK_STEP)["conclusion"] = json!("failure")),
            format!("acceptance step did not conclude success: {PARK_STEP}"),
        ),
        (
            Box::new(|run: &mut Value| {
                step(run, VERIFY_IMAGE_STEP)["conclusion"] = json!("skipped")
            }),
            format!("acceptance step did not conclude success: {VERIFY_IMAGE_STEP}"),
        ),
        (
            Box::new(|run: &mut Value| {
                acceptance_job(run)["steps"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|s| s["name"] != HARNESS_STEP)
            }),
            format!("acceptance step did not conclude success: {HARNESS_STEP}"),
        ),
        (
            Box::new(|run: &mut Value| acceptance_job(run)["conclusion"] = json!("failure")),
            "acceptance job did not conclude success".into(),
        ),
        (
            Box::new(|run: &mut Value| acceptance_job(run)["status"] = json!("in_progress")),
            "acceptance job did not conclude success".into(),
        ),
        (
            Box::new(|run: &mut Value| {
                let job = acceptance_job(run).clone();
                run["jobs"].as_array_mut().unwrap().push(job);
            }),
            "run does not contain exactly one acceptance job".into(),
        ),
        (
            Box::new(|run: &mut Value| acceptance_job(run)["name"] = json!("other")),
            "run does not contain exactly one acceptance job".into(),
        ),
    ];
    for (mutate, message) in cases {
        assert_eq!(refused_run(mutate), format!("{P}{message}"));
    }
}

#[test]
fn last_step_with_the_same_name_wins() {
    let msg = refused_run(|run: &mut Value| {
        acceptance_job(run)["steps"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": PARK_STEP, "conclusion": "failure"}));
    });
    assert_eq!(
        msg,
        format!("{P}acceptance step did not conclude success: {PARK_STEP}")
    );

    // A failing step that comes before the passing one with the same name does not count.
    let s = summary();
    let mut run = good_run(&s);
    acceptance_job(&mut run)["steps"]
        .as_array_mut()
        .unwrap()
        .insert(0, json!({"name": HARNESS_STEP, "conclusion": "failure"}));
    let dir = evidence(&s);
    assert!(validate_gke_acceptance("keep", &s, run.as_object().unwrap(), dir.path()).is_ok());
}

#[test]
fn evidence_mutations_are_refused() {
    let cases: Vec<EvidenceCase> = vec![
        (
            Box::new(|dir: &Path| put(dir, "keep/verdict.txt", "[keep] FAIL: readyz\n")),
            "harness verdict is not the exact PASS line".into(),
        ),
        (
            Box::new(|dir: &Path| put(dir, "results.txt", "keep FAIL\n")),
            "harness results do not record exactly one PASS for this app".into(),
        ),
        (
            Box::new(|dir: &Path| put(dir, "results.txt", "keep PASS\nrelay PASS\n")),
            "harness results do not record exactly one PASS for this app".into(),
        ),
        (
            Box::new(|dir: &Path| {
                put(
                    dir,
                    "keep/manifests.yaml",
                    "image: ghcr.io/faberline/keep:latest\n",
                )
            }),
            "rendered manifests do not deploy the candidate image".into(),
        ),
        (
            // The negative test for the digest check: a different digest is refused.
            Box::new(|dir: &Path| {
                put(
                    dir,
                    "keep/manifests.yaml",
                    format!("image: {IMAGE_OWNER}/keep@sha256:{}\n", "a".repeat(64)),
                )
            }),
            "rendered manifests do not deploy the candidate image".into(),
        ),
        (
            Box::new(|dir: &Path| put(dir, "keep/manifests.yaml", [0xff_u8, 0xfe])),
            "rendered manifests are not UTF-8".into(),
        ),
    ];
    for (mutate, message) in cases {
        assert_eq!(refused_evidence(mutate), format!("{P}{message}"));
    }
}

#[test]
fn missing_evidence_files_are_refused() {
    let cases: Vec<MissingCase> = vec![
        (
            Box::new(|dir: &Path| std::fs::remove_file(dir.join("keep/teardown.log")).unwrap()),
            "teardown log must be a regular file: ",
        ),
        (
            Box::new(|dir: &Path| std::fs::remove_file(dir.join("keep/verdict.txt")).unwrap()),
            "harness verdict must be a regular file: ",
        ),
        (
            Box::new(|dir: &Path| std::fs::remove_file(dir.join("results.txt")).unwrap()),
            "harness results must be a regular file: ",
        ),
        (
            Box::new(|dir: &Path| std::fs::remove_file(dir.join("keep/manifests.yaml")).unwrap()),
            "rendered manifests must be a regular file: ",
        ),
    ];
    for (mutate, prefix) in cases {
        let msg = refused_evidence(mutate);
        assert!(msg.starts_with(&format!("{P}{prefix}")), "{msg}");
    }
}

#[test]
fn results_are_checked_before_verdict() {
    let msg = refused_evidence(|dir: &Path| {
        put(dir, "results.txt", "keep FAIL\n");
        std::fs::remove_file(dir.join("keep/verdict.txt")).unwrap();
    });
    assert_eq!(
        msg,
        format!("{P}harness results do not record exactly one PASS for this app")
    );
}

#[test]
fn evidence_directory_must_be_a_real_directory() {
    let s = summary();
    let run = good_run(&s);
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let file = root.path().join("file");
    std::fs::write(&file, "x").unwrap();
    for path in [missing.as_path(), file.as_path()] {
        let msg = validate_gke_acceptance("keep", &s, run.as_object().unwrap(), path)
            .unwrap_err()
            .to_string();
        assert_eq!(msg, format!("{P}evidence directory is invalid"));
    }
    #[cfg(unix)]
    {
        let real = evidence(&s);
        let link = root.path().join("link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let msg = validate_gke_acceptance("keep", &s, run.as_object().unwrap(), &link)
            .unwrap_err()
            .to_string();
        assert_eq!(msg, format!("{P}evidence directory is invalid"));
    }
}
