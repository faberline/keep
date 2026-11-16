//! Test support: a candidate manifest that passes `validate_candidate`. Port of the script's `fixture_candidate`.

use crate::candidate::{
    app_targets, candidate_schema, final_jobs, workflow_ref, IMAGE_OWNER, REPOSITORY,
};
use crate::util::sha256_hex;
use serde_json::{json, Value};

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
