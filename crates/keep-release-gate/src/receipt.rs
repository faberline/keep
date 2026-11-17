//! The gke-acceptance check of the release receipt. Port of `validate_gke_acceptance` in the receipt script.

use crate::candidate::{
    receipt_schema, validate_candidate, CandidateSummary, IMAGE_OWNER, REPOSITORY,
};
use crate::refusal::{refuse, Refusal, Result};
use crate::util::{is_int, load_json, read_regular, sha256_hex};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// The job of the gke-acceptance workflow that deploys and verifies the candidate image.
pub const ACCEPTANCE_JOB: &str = "deploy + verify on GKE";
/// The step of the acceptance job that verifies the prebuilt image input.
pub const VERIFY_IMAGE_STEP: &str = "Verify prebuilt image input";
/// The step of the acceptance job that runs the acceptance harness.
pub const HARNESS_STEP: &str = "Run acceptance harness";
/// The step of the acceptance job that parks the node pool.
pub const PARK_STEP: &str = "Park node pool (belt and suspenders)";
/// The harness fields that a passing run reports.
pub const HARNESS_FIELDS: [&str; 3] = ["readyz", "round_trip", "durability"];

/// A checked gke-acceptance run: its id, its URL and the hashes of its evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GkeAcceptance {
    /// `gha-<run id>-<attempt>`.
    pub run_id: String,
    /// The URL of the run in this repository.
    pub run_url: String,
    /// Keys: `manifests_sha256`, `results_sha256`, `verdict_sha256`.
    pub evidence: BTreeMap<String, String>,
}

/// A JSON integer greater than zero, or `None`.
fn positive_int(value: &Value) -> Option<u64> {
    if is_int(value) {
        value.as_u64().filter(|n| *n > 0)
    } else {
        None
    }
}

/// Check a gke-acceptance run record and its evidence directory. The checks run in the same order, with the same messages, as the receipt script.
pub fn validate_gke_acceptance(
    app: &str,
    candidate: &CandidateSummary,
    gh_run: &Map<String, Value>,
    evidence_dir: &Path,
) -> Result<GkeAcceptance> {
    let image = format!("{IMAGE_OWNER}/{app}@{}", candidate.root_digest);
    let commit = candidate.commit.as_str();
    const REQUIRED: [&str; 10] = [
        "databaseId",
        "attempt",
        "conclusion",
        "status",
        "url",
        "headSha",
        "event",
        "workflowName",
        "displayTitle",
        "jobs",
    ];
    if !REQUIRED.iter().all(|key| gh_run.contains_key(*key)) {
        return refuse("gh run record is missing fields; use --json databaseId,attempt,conclusion,status,url,headSha,event,workflowName,displayTitle,jobs");
    }
    let (Some(run_number), Some(attempt)) = (
        positive_int(&gh_run["databaseId"]),
        positive_int(&gh_run["attempt"]),
    ) else {
        return refuse("gh run identity is invalid");
    };
    if gh_run["workflowName"].as_str() != Some("gke-acceptance") {
        return refuse("run is not a gke-acceptance workflow run");
    }
    if gh_run["event"].as_str() != Some("workflow_dispatch") {
        return refuse("run was not dispatched by hand");
    }
    if gh_run["status"].as_str() != Some("completed")
        || gh_run["conclusion"].as_str() != Some("success")
    {
        return refuse("run did not conclude success");
    }
    if gh_run["headSha"].as_str() != Some(commit) {
        return refuse("run head commit is not the candidate commit");
    }
    let run_url = format!("https://github.com/{REPOSITORY}/actions/runs/{run_number}");
    if gh_run["url"].as_str() != Some(run_url.as_str()) {
        return refuse("run URL does not belong to this repository and run");
    }
    let title = format!("gke-acceptance {app} @ {commit} image={image}");
    if gh_run["displayTitle"].as_str() != Some(title.as_str()) {
        return refuse("run was not dispatched for this app with the candidate image");
    }
    let Some(jobs) = gh_run["jobs"].as_array().filter(|jobs| !jobs.is_empty()) else {
        return refuse("run has no job inventory");
    };
    let acceptance: Vec<usize> = jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| job.get("name").and_then(Value::as_str) == Some(ACCEPTANCE_JOB))
        .map(|(index, _)| index)
        .collect();
    let [job_index] = acceptance[..] else {
        return refuse("run does not contain exactly one acceptance job");
    };
    let job = &jobs[job_index];
    if job["status"].as_str() != Some("completed") || job["conclusion"].as_str() != Some("success")
    {
        return refuse("acceptance job did not conclude success");
    }
    let no_steps: &[Value] = &[];
    let steps: BTreeMap<&str, &Value> = job["steps"]
        .as_array()
        .map_or(no_steps, Vec::as_slice)
        .iter()
        .filter_map(|step| Some((step.get("name")?.as_str()?, step)))
        .collect();
    for name in [VERIFY_IMAGE_STEP, HARNESS_STEP, PARK_STEP] {
        let conclusion = steps.get(name).and_then(|step| step["conclusion"].as_str());
        if conclusion != Some("success") {
            return refuse(format!("acceptance step did not conclude success: {name}"));
        }
    }
    for (index, other) in jobs.iter().enumerate() {
        if index != job_index
            && (other.as_object().is_none() || other["conclusion"].as_str() != Some("skipped"))
        {
            return refuse("a build job ran; the candidate image must be the only image in play");
        }
    }
    let is_real_dir = std::fs::symlink_metadata(evidence_dir)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false);
    if !is_real_dir {
        return refuse("evidence directory is invalid");
    }
    let results = read_regular(&evidence_dir.join("results.txt"), "harness results")?;
    if results != format!("{app} PASS\n").as_bytes() {
        return refuse("harness results do not record exactly one PASS for this app");
    }
    let verdict = read_regular(
        &evidence_dir.join(app).join("verdict.txt"),
        "harness verdict",
    )?;
    if verdict != format!("[{app}] PASS: readyz + round-trip + durability\n").as_bytes() {
        return refuse("harness verdict is not the exact PASS line");
    }
    let manifests = read_regular(
        &evidence_dir.join(app).join("manifests.yaml"),
        "rendered manifests",
    )?;
    let Ok(manifest_text) = std::str::from_utf8(&manifests) else {
        return refuse("rendered manifests are not UTF-8");
    };
    let deployed = format!("image: {image}");
    if !manifest_text.lines().any(|line| line.trim() == deployed) {
        return refuse("rendered manifests do not deploy the candidate image");
    }
    read_regular(&evidence_dir.join(app).join("teardown.log"), "teardown log")?;

    let evidence = BTreeMap::from([
        ("manifests_sha256".to_string(), sha256_hex(&manifests)),
        ("results_sha256".to_string(), sha256_hex(&results)),
        ("verdict_sha256".to_string(), sha256_hex(&verdict)),
    ]);
    Ok(GkeAcceptance {
        run_id: format!("gha-{run_number}-{attempt}"),
        run_url,
        evidence,
    })
}

/// The files that a receipt run reads. `run`, `images`, `acceptance` and `cleanup` belong to the dropped `gcp` backend: they are accepted and ignored.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReceiptInputs<'a> {
    pub run: Option<&'a Path>,
    pub images: Option<&'a Path>,
    pub acceptance: Option<&'a Path>,
    pub cleanup: Option<&'a Path>,
    pub gh_run: Option<&'a Path>,
    pub evidence_dir: Option<&'a Path>,
}

/// Rebuild every object with its keys in sorted order, at every level.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let ordered: BTreeMap<&String, Value> =
                map.iter().map(|(key, item)| (key, sorted(item))).collect();
            Value::Object(
                ordered
                    .into_iter()
                    .map(|(key, item)| (key.clone(), item))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// The bytes of a receipt: compact JSON, keys sorted at every level, one trailing newline.
pub fn receipt_bytes(receipt: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&sorted(receipt)).expect("a JSON value serializes");
    bytes.push(b'\n');
    bytes
}

/// Whether `path` is a symbolic link (a missing path is not).
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// Check the inputs, write the receipt and its sidecar, and return the receipt. Port of `make_receipt` in the receipt script (the `gke-acceptance` backend only).
pub fn make_receipt(
    app: &str,
    backend: &str,
    candidate_manifest: &Path,
    output: &Path,
    inputs: &ReceiptInputs,
) -> Result<Value> {
    if app != "keep" {
        return refuse(format!("unknown shared-script app: {app} (known: keep)"));
    }
    if backend != "gke-acceptance" {
        return refuse(format!(
            "keep proves GKE acceptance with backend gke-acceptance, not {backend}"
        ));
    }
    let (candidate_raw, candidate_map) = load_json(candidate_manifest, "candidate manifest")?;
    let candidate = validate_candidate(app, &candidate_raw, &candidate_map)?;
    let (Some(gh_run_path), Some(evidence_dir)) = (inputs.gh_run, inputs.evidence_dir) else {
        return refuse("backend gke-acceptance needs --gh-run and --evidence-dir");
    };
    let (gh_raw, gh_map) = load_json(gh_run_path, "gh run record")?;
    let acceptance = validate_gke_acceptance(app, &candidate, &gh_map, evidence_dir)?;

    let mut evidence = Map::new();
    for (key, digest) in &acceptance.evidence {
        evidence.insert(key.clone(), json!(digest));
    }
    evidence.insert("gh_run_sha256".to_string(), json!(sha256_hex(&gh_raw)));
    let functional: Map<String, Value> = HARNESS_FIELDS
        .iter()
        .map(|field| (field.to_string(), json!("passed")))
        .collect();
    let receipt = json!({
        "schema": receipt_schema(app),
        "complete": true,
        "result": "passed",
        "candidate": candidate.to_value(),
        "gke": {
            "backend": "gke-acceptance",
            "run_id": acceptance.run_id,
            "run_url": acceptance.run_url,
            "image": format!("{IMAGE_OWNER}/{app}@{}", candidate.root_digest),
            "image_provenance": "prebuilt",
            "functional": functional,
            "cleanup": {
                "schema": "axiom.gke-harness.cleanup.v1",
                "status": "clean",
                "namespace_deleted": true,
                "node_pool_parked": true,
            },
        },
        "evidence": evidence,
        "redaction": {
            "kubeconfig_retained": false,
            "token_retained": false,
            "secret_retained": false,
            "cluster_identity_retained": false,
            "command_output_retained": false,
        },
    });

    let bytes = receipt_bytes(&receipt);
    if is_symlink(output) {
        return refuse("output must not be a symlink");
    }
    let file_name = format!("{app}-gke-receipt.json");
    if output.file_name().and_then(|name| name.to_str()) != Some(file_name.as_str()) {
        return refuse(format!("output must be named {file_name}"));
    }
    let cannot_write = |path: &Path, error: std::io::Error| {
        Refusal(format!("cannot write {}: {error}", path.display()))
    };
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| cannot_write(parent, error))?;
    }
    std::fs::write(output, &bytes).map_err(|error| cannot_write(output, error))?;

    let sidecar = output.with_file_name(format!("{file_name}.sha256"));
    if is_symlink(&sidecar) {
        return refuse("sidecar output must not be a symlink");
    }
    let line = format!("{}  {file_name}\n", sha256_hex(&bytes));
    std::fs::write(&sidecar, line).map_err(|error| cannot_write(&sidecar, error))?;
    Ok(receipt)
}
