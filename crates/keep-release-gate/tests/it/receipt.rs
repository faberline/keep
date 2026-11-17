//! Tests for `make_receipt`, `receipt_bytes` and the receipt sidecar. The parity test compares the bytes with the output of the Python script.

use keep_release_gate::candidate::IMAGE_OWNER;
use keep_release_gate::fixtures::{
    candidate, gke_acceptance, write_harness_evidence, FIXTURE_COMMIT,
};
use keep_release_gate::receipt::{make_receipt, receipt_bytes, ReceiptInputs};
use keep_release_gate::refusal::Refusal;
use keep_release_gate::util::sha256_hex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const P: &str = "GKE receipt refused: ";

/// A temporary set of inputs for one receipt run of the fixture keep release.
struct Fixture {
    _dir: tempfile::TempDir,
    manifest: PathBuf,
    gh_run: PathBuf,
    evidence: PathBuf,
    output: PathBuf,
}

impl Fixture {
    /// Write the fixture candidate, run record and harness evidence. The output is `out/keep-gke-receipt.json`.
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let candidate_value = candidate("keep", "0.4.13", FIXTURE_COMMIT, "12345", "1");
        let root_digest = candidate_value["image"]["root_digest"]
            .as_str()
            .unwrap()
            .to_string();
        let image = format!("{IMAGE_OWNER}/keep@{root_digest}");

        let manifest = dir.path().join("candidate.json");
        std::fs::write(&manifest, serde_json::to_vec(&candidate_value).unwrap()).unwrap();

        let gh_run = dir.path().join("gh-run.json");
        let run_value = gke_acceptance("keep", FIXTURE_COMMIT, &image);
        std::fs::write(&gh_run, serde_json::to_vec(&run_value).unwrap()).unwrap();

        let evidence = dir.path().join("evidence");
        std::fs::create_dir_all(&evidence).unwrap();
        write_harness_evidence("keep", &evidence, &image).unwrap();

        let output = dir.path().join("out").join("keep-gke-receipt.json");
        Fixture {
            _dir: dir,
            manifest,
            gh_run,
            evidence,
            output,
        }
    }

    fn inputs(&self) -> ReceiptInputs<'_> {
        ReceiptInputs {
            gh_run: Some(&self.gh_run),
            evidence_dir: Some(&self.evidence),
            ..Default::default()
        }
    }

    /// Run the receipt for the keep gke-acceptance backend.
    fn run(&self) -> Result<Value, Refusal> {
        make_receipt(
            "keep",
            "gke-acceptance",
            &self.manifest,
            &self.output,
            &self.inputs(),
        )
    }

    /// Read the candidate manifest back as JSON, change it with `mutate`, and write it again.
    fn mutate_candidate(&self, mutate: fn(&mut Value)) {
        let mut value: Value =
            serde_json::from_slice(&std::fs::read(&self.manifest).unwrap()).unwrap();
        mutate(&mut value);
        std::fs::write(&self.manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    }
}

#[test]
fn receipt_for_the_fixture_is_written() {
    let fx = Fixture::new();
    let value = fx.run().unwrap();

    assert_eq!(value["gke"]["run_id"], "gha-99001-1");
    assert_eq!(value["gke"]["backend"], "gke-acceptance");
    assert_eq!(value["redaction"]["kubeconfig_retained"], false);

    let mut keys: Vec<&str> = value["evidence"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "gh_run_sha256",
            "manifests_sha256",
            "results_sha256",
            "verdict_sha256"
        ]
    );

    assert_eq!(std::fs::read(&fx.output).unwrap(), receipt_bytes(&value));
}

#[test]
fn sidecar_has_the_hash_line() {
    let fx = Fixture::new();
    fx.run().unwrap();

    let bytes = std::fs::read(&fx.output).unwrap();
    let sidecar =
        std::fs::read_to_string(fx.output.with_file_name("keep-gke-receipt.json.sha256")).unwrap();
    assert_eq!(
        sidecar,
        format!("{}  keep-gke-receipt.json\n", sha256_hex(&bytes))
    );
}

#[test]
fn kubeconfig_is_not_hashed_or_copied() {
    let fx = Fixture::new();
    let value = fx.run().unwrap();

    let text = std::fs::read_to_string(&fx.output).unwrap();
    let kubeconfig = std::fs::read(fx.evidence.join("kubeconfig")).unwrap();
    assert!(!text.contains(&sha256_hex(&kubeconfig)));
    assert!(!text.contains("apiVersion: v1"));
    assert!(!text.contains("kind: Config"));
    assert!(value["evidence"]
        .as_object()
        .unwrap()
        .keys()
        .all(|key| !key.contains("kubeconfig")));
}

#[test]
fn receipt_bytes_equal_the_python_bytes() {
    let parity = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parity");
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path().join("keep-gke-receipt.json");
    let gh_run = parity.join("gh-run.json");
    let evidence = parity.join("evidence");
    let inputs = ReceiptInputs {
        gh_run: Some(&gh_run),
        evidence_dir: Some(&evidence),
        ..Default::default()
    };
    make_receipt(
        "keep",
        "gke-acceptance",
        &parity.join("final-candidate-manifest.json"),
        &output,
        &inputs,
    )
    .unwrap();

    assert_eq!(
        std::fs::read(&output).unwrap(),
        std::fs::read(parity.join("keep-gke-receipt.json")).unwrap()
    );
    assert_eq!(
        std::fs::read(tmp.path().join("keep-gke-receipt.json.sha256")).unwrap(),
        std::fs::read(parity.join("keep-gke-receipt.json.sha256")).unwrap()
    );
}

#[test]
fn receipt_bytes_sorts_keys_compactly() {
    let value = json!({"b": 1, "a": {"d": 2, "c": [{"z": 1, "y": 2}]}});
    let mut expected = br#"{"a":{"c":[{"y":2,"z":1}],"d":2},"b":1}"#.to_vec();
    expected.push(b'\n');
    assert_eq!(receipt_bytes(&value), expected);
}

#[test]
fn candidate_mutations_are_refused() {
    type Mutation = fn(&mut Value);
    let cases: [(Mutation, &str); 8] = [
        (
            |v| v["jobs"]["verify-candidate"] = json!("failure"),
            "candidate does not bind every required successful job",
        ),
        (
            |v| {
                v["jobs"]
                    .as_object_mut()
                    .unwrap()
                    .remove("keep-release-gates");
            },
            "candidate does not bind every required successful job",
        ),
        (
            |v| v["image"]["root_digest"] = v["image"]["amd64_digest"].clone(),
            "candidate root and child digests must be distinct",
        ),
        (
            |v| v["image"]["repository"] = json!("ghcr.io/faberline/relay"),
            "candidate image repository changed",
        ),
        (
            |v| v["tag"] = json!("keep@0.4.14"),
            "candidate release tag changed",
        ),
        (
            |v| v["source_ref"] = json!("refs/heads/feature"),
            "candidate was not built from main",
        ),
        (
            |v| v["candidate_tag"] = json!("release-candidate-1-1"),
            "candidate image tag is not run scoped",
        ),
        (
            |v| {
                let mut extra = v["artifacts"][0].clone();
                extra["target"] = json!("aarch64-apple-darwin");
                v["artifacts"].as_array_mut().unwrap().push(extra);
            },
            "candidate artifact targets changed",
        ),
    ];
    for (mutate, expected) in cases {
        let fx = Fixture::new();
        fx.mutate_candidate(mutate);
        let text = fx.run().unwrap_err().to_string();
        assert_eq!(text, format!("{P}{expected}"));
    }
}

#[test]
fn changed_digest_in_manifests_is_refused() {
    let fx = Fixture::new();
    let wrong = format!("image: {IMAGE_OWNER}/keep@sha256:{}\n", "a".repeat(64));
    std::fs::write(fx.evidence.join("keep").join("manifests.yaml"), wrong).unwrap();
    assert_eq!(
        fx.run().unwrap_err().to_string(),
        format!("{P}rendered manifests do not deploy the candidate image")
    );
}

#[test]
fn wrong_backend_is_refused() {
    let fx = Fixture::new();
    let text = make_receipt("keep", "gcp", &fx.manifest, &fx.output, &fx.inputs())
        .unwrap_err()
        .to_string();
    assert_eq!(
        text,
        format!("{P}keep proves GKE acceptance with backend gke-acceptance, not gcp")
    );
}

#[test]
fn unknown_app_is_refused() {
    let fx = Fixture::new();
    let text = make_receipt(
        "loom",
        "gke-acceptance",
        &fx.manifest,
        &fx.output,
        &fx.inputs(),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(
        text,
        format!("{P}unknown shared-script app: loom (known: keep)")
    );
}

#[test]
fn missing_gh_inputs_are_refused() {
    let fx = Fixture::new();
    let text = make_receipt(
        "keep",
        "gke-acceptance",
        &fx.manifest,
        &fx.output,
        &ReceiptInputs::default(),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(
        text,
        format!("{P}backend gke-acceptance needs --gh-run and --evidence-dir")
    );
}

#[test]
fn wrong_output_name_is_refused() {
    let mut fx = Fixture::new();
    fx.output = fx.output.with_file_name("receipt.json");
    assert_eq!(
        fx.run().unwrap_err().to_string(),
        format!("{P}output must be named keep-gke-receipt.json")
    );
    assert!(!fx.output.exists());
}

#[cfg(unix)]
#[test]
fn symlink_output_is_refused() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.output.parent().unwrap()).unwrap();
    let target = fx.output.with_file_name("elsewhere.json");
    std::fs::write(&target, b"{}").unwrap();
    std::os::unix::fs::symlink(&target, &fx.output).unwrap();
    assert_eq!(
        fx.run().unwrap_err().to_string(),
        format!("{P}output must not be a symlink")
    );
}

#[cfg(unix)]
#[test]
fn symlink_sidecar_is_refused() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.output.parent().unwrap()).unwrap();
    let target = fx.output.with_file_name("elsewhere.sha256");
    std::fs::write(&target, b"").unwrap();
    std::os::unix::fs::symlink(
        &target,
        fx.output.with_file_name("keep-gke-receipt.json.sha256"),
    )
    .unwrap();
    assert_eq!(
        fx.run().unwrap_err().to_string(),
        format!("{P}sidecar output must not be a symlink")
    );
}

#[test]
fn unwritable_output_directory_is_refused() {
    let mut fx = Fixture::new();
    let blocker = fx.output.with_file_name("blocker");
    std::fs::create_dir_all(fx.output.parent().unwrap()).unwrap();
    std::fs::write(&blocker, b"not a directory").unwrap();
    fx.output = blocker.join("keep-gke-receipt.json");
    let text = fx.run().unwrap_err().to_string();
    assert!(
        text.starts_with(&format!("{P}cannot write ")),
        "unexpected refusal: {text}"
    );
}
