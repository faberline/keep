//! CI contract: the release image and its workflows keep the properties that
//! the release depends on. These tests read repository files as text; no
//! workflow or image is run.

use std::path::Path;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|error| panic!("read {}: {error}", full.display()))
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The release image runs as a non-root user: its runtime stage is the
/// distroless `:nonroot` image (uid 65532), pinned by digest, and no `USER`
/// line sets root.
#[test]
fn release_image_runs_as_non_root() {
    let dockerfile = read("Dockerfile.release");
    let runtime_from = dockerfile
        .lines()
        .rfind(|line| line.trim_start().starts_with("FROM "))
        .expect("Dockerfile.release has no FROM line");
    let digest: String = runtime_from
        .split(":nonroot@sha256:")
        .nth(1)
        .unwrap_or("")
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    assert!(
        runtime_from.contains(":nonroot@sha256:") && digest.len() == 64,
        "runtime FROM is not a digest-pinned :nonroot image: {runtime_from}"
    );
    for line in dockerfile.lines() {
        let trimmed = line.trim().to_ascii_lowercase();
        if let Some(user) = trimmed.strip_prefix("user ") {
            let uid = user.trim().split(':').next().unwrap_or("");
            assert!(
                uid != "root" && uid != "0",
                "Dockerfile.release runs as root: {line}"
            );
        }
    }
}

/// The release workflow calls the shared aw release workflow, pinned to a
/// full commit SHA. The SBOM and cosign steps live there, not in keep.
#[test]
fn release_workflow_uses_shared_service_release() {
    let workflow = read(".github/workflows/keep-release.yml");
    let marker = "uses: faberline/aw/.github/workflows/service-release.yml@";
    let at = workflow.find(marker).unwrap_or_else(|| {
        panic!("keep-release.yml does not call the shared service-release workflow")
    });
    let sha: String = workflow[at + marker.len()..].chars().take(40).collect();
    assert!(
        is_hex40(&sha),
        "shared service-release is not pinned to a 40-hex SHA: {sha}"
    );
}

/// The reproducible-image workflow runs by hand only, has no push or pull
/// request trigger, is not the required `gate` check, and pins every action.
#[test]
fn reproducible_image_workflow_is_manual_and_pinned() {
    let workflow = read(".github/workflows/keep-reproducible-image.yml");
    assert!(
        workflow.contains("workflow_dispatch:"),
        "keep-reproducible-image.yml is not a manual workflow"
    );
    assert!(
        !workflow.contains("pull_request"),
        "keep-reproducible-image.yml must not run on pull requests"
    );
    assert!(
        !workflow.contains("push:"),
        "keep-reproducible-image.yml must not run on push"
    );
    assert!(
        !workflow
            .lines()
            .any(|line| matches!(line.trim(), "gate:" | "name: gate")),
        "keep-reproducible-image.yml must not define the required gate check"
    );
    assert!(
        workflow.lines().any(|line| line == "  reproducible:"),
        "keep-reproducible-image.yml has no job `reproducible`"
    );

    let mut pinned = 0;
    for line in workflow.lines() {
        let trimmed = line.trim();
        let trimmed = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        if let Some(target) = trimmed.strip_prefix("uses:") {
            let (_, reference) = target
                .trim()
                .split_once('@')
                .unwrap_or_else(|| panic!("action without a ref: {line}"));
            let sha = reference.split_whitespace().next().unwrap_or("");
            assert!(
                is_hex40(sha),
                "action is not pinned to a 40-hex SHA: {line}"
            );
            pinned += 1;
        }
    }
    assert!(
        pinned >= 3,
        "expected at least 3 pinned uses: lines, found {pinned}"
    );
}
