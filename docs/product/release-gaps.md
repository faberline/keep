# Release gaps against release-tooling.md

This page walks `docs/standards/release-tooling.md` in faberline/aw (sections
"The promotion workflow" and "The milestone gate") against
`.github/workflows/keep-release.yml` and `.github/workflows/keep-release-candidate.yml`.
It changes no workflow. The gates of the promotion workflow run inside the shared
workflow `faberline/aw/.github/workflows/service-release.yml`, which keep calls from
the job `release`; this page takes that standard's word for them and does not
re-run them here. The paid GKE failure drills and the 24-hour soak are not rows
here: they wait for the owner and no issue tracks them yet.

## Gates

| Gate | Where keep does it | Status |
|---|---|---|
| Dispatch event, N.N.N version, exact tag ref and workflow identity | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Receipt and sidecar are canonical base64 with the given SHA-256 | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Artifact verifier passes in candidate mode for the exact run ID and attempt | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Contract binds commit, run ID, attempt, receipt hashes and three digests | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| An existing public release is only verified, nothing is written | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Candidate artifact has the final manifest and its sidecar | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Semver tag and latest tag rules, read back after the write | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Promotion runs imagetools create from the candidate root and has no build command | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Release carries tarballs, sidecars, SPDX files, receipt and notes | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Artifact verifier passes in public mode after the release exists | `.github/workflows/keep-release.yml` `service-release.yml@` | present |
| Candidate identity | `.github/workflows/keep-release-candidate.yml` `identity:` | present |
| Build per target | `.github/workflows/keep-release-candidate.yml` `build:` | present |
| Keep release gates | `.github/workflows/keep-release-candidate.yml` `keep-release-gates:` | present |
| Candidate image build and attest | `.github/workflows/keep-release-candidate.yml` `ghcr-image-and-attest:` | present |
| Manifest binds candidate inputs | `.github/workflows/keep-release-candidate.yml` `manifest:` | present |
| Verify candidate supply chain | `.github/workflows/keep-release-candidate.yml` `verify-candidate:` | present |
| Final candidate receipt | `.github/workflows/keep-release-candidate.yml` `result:` | present |
| release.caller: one job that uses service-release.yml at a 40-hex commit | `.github/workflows/keep-release.yml` `uses: faberline/aw/.github/workflows/service-release.yml@` | present |
| release.caller-shape: concurrency group does not cancel | `.github/workflows/keep-release.yml` `cancel-in-progress: false` | present |
| release.pin: commit pinned with the aw version comment | `.github/workflows/keep-release.yml` `# aw 0.` | present |
| Milestone gate contract | `.github/aw-release.json` `goal_sha256` | present |
| Milestone gate policy | `.github/aw-release-gates.json` `definition_sha` | present |
| release.own-copy: no copy of the shared release scripts | `scripts/release/` still a copy | missing |
| [release] table in aw.toml replaces apps.sh | `aw.toml` has no `[release]` table | missing |

## Missing gates

- release.own-copy: no copy of the shared release scripts: delete the copies in `scripts/release/` once aw supplies them (aw says "not started"). Until then, record `[[standards.release.exception]]` in `aw.toml`.
- [release] table in aw.toml replaces apps.sh: move the app facts from `scripts/release/apps.sh` to a `[release]` table in `aw.toml` when aw's scripts read it.

`aw release check` is not built yet, so this page is the manual check.
