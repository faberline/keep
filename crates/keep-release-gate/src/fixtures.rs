//! Test support: a candidate manifest that passes `validate_candidate`. Port of the script's `fixture_candidate`.

use crate::candidate::{
    app_targets, candidate_schema, final_jobs, workflow_ref, IMAGE_OWNER, REPOSITORY,
};
use crate::receipt::{ACCEPTANCE_JOB, HARNESS_STEP, PARK_STEP, VERIFY_IMAGE_STEP};
use crate::util::sha256_hex;
use serde_json::{json, Value};
use std::path::Path;

pub const FIXTURE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A candidate manifest for `app`. Panics on an unknown app: fixtures are test support.
pub fn candidate(app: &str, version: &str, commit: &str, run_id: &str, attempt: &str) -> Value {
    let digest = |seed: &str| format!("sha256:{}", sha256_hex(format!("{app}-{seed}").as_bytes()));
    let artifacts: Vec<Value> = app_targets(app)
        .expect("fixture app is known")
        .iter()
        .map(|target| {
            json!({
                "target": target,
                "archive": format!("{app}-{target}.tar.gz"),
                "archive_sha256": "0".repeat(64),
                "sidecar": format!("{app}-{target}.tar.gz.sha256"),
                "sidecar_sha256": "1".repeat(64),
            })
        })
        .collect();
    json!({
        "schema": candidate_schema(app),
        "repository": REPOSITORY,
        "workflow_path": format!(".github/workflows/{app}-release-candidate.yml"),
        "workflow_id": 42,
        "run_id": run_id,
        "run_attempt": attempt,
        "run_url": format!("https://github.com/{REPOSITORY}/actions/runs/{run_id}/attempts/{attempt}"),
        "source_ref": "refs/heads/main",
        "workflow_ref": workflow_ref(app),
        "commit": commit,
        "version": version,
        "tag": format!("{app}@{version}"),
        "candidate_tag": format!("release-candidate-{run_id}-{attempt}"),
        "pr": {"number": 7, "url": format!("https://github.com/{REPOSITORY}/pull/7")},
        "image": {
            "repository": format!("{IMAGE_OWNER}/{app}"),
            "root_digest": digest("root"),
            "amd64_digest": digest("amd64"),
            "arm64_digest": digest("arm64"),
        },
        "artifacts": artifacts,
        "sboms": {
            "amd64": {"file": "spdx-amd64.json", "sha256": "2".repeat(64)},
            "arm64": {"file": "spdx-arm64.json", "sha256": "3".repeat(64)},
        },
        "jobs": Value::Object(final_jobs(app)),
    })
}

/// A gke-acceptance run record that `validate_gke_acceptance` accepts. Port of the script's `fixture_gke_acceptance`.
pub fn gke_acceptance(app: &str, commit: &str, image: &str) -> Value {
    let step = |name: &str| json!({"name": name, "status": "completed", "conclusion": "success", "number": 1});
    let skipped = |name: &str| json!({"name": name, "status": "completed", "conclusion": "skipped", "steps": []});
    json!({
        "databaseId": 99001,
        "attempt": 1,
        "conclusion": "success",
        "status": "completed",
        "event": "workflow_dispatch",
        "workflowName": "gke-acceptance",
        "url": format!("https://github.com/{REPOSITORY}/actions/runs/99001"),
        "headSha": commit,
        "displayTitle": format!("gke-acceptance {app} @ {commit} image={image}"),
        "jobs": [
            skipped("build-keep"),
            skipped("build-defer"),
            skipped("build-relay"),
            skipped("build-loom"),
            {
                "name": ACCEPTANCE_JOB,
                "status": "completed",
                "conclusion": "success",
                "steps": [
                    step("Set up job"),
                    step("Run actions/checkout@v4"),
                    step(VERIFY_IMAGE_STEP),
                    step("Prepare evidence directory"),
                    step("Authenticate to GCP (Workload Identity Federation)"),
                    step("Set up gcloud + GKE auth plugin"),
                    step(HARNESS_STEP),
                    step("Upload evidence bundle"),
                    step(PARK_STEP),
                ],
            },
        ],
    })
}

/// Write the harness evidence of `app` into `dir`. Port of the script's `write_harness_evidence`.
pub fn write_harness_evidence(app: &str, dir: &Path, image: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join(app))?;
    std::fs::write(
        dir.join("kubeconfig"),
        "apiVersion: v1\nkind: Config\nusers: [{name: redacted}]\n",
    )?;
    std::fs::write(dir.join("cluster-name.txt"), "axiom-acceptance\n")?;
    std::fs::write(dir.join("results.txt"), format!("{app} PASS\n"))?;
    std::fs::write(
        dir.join(app).join("verdict.txt"),
        format!("[{app}] PASS: readyz + round-trip + durability\n"),
    )?;
    std::fs::write(
        dir.join(app).join("manifests.yaml"),
        format!(
            "apiVersion: apps/v1\nkind: StatefulSet\nspec:\n  template:\n    spec:\n      containers:\n      - name: {app}\n        image: {image}\n"
        ),
    )?;
    std::fs::write(dir.join(app).join("teardown.log"), "namespace deleted\n")
}
