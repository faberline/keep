#!/usr/bin/env bash
# Release metadata table for keep shared by scripts/release/*.sh. Source it; never run it.
#
# keep rides the candidate-first release path:
# landed main -> immutable candidate -> digest-pinned GKE acceptance ->
# protected annotated tag -> no-rebuild promotion.
RELEASE_REPO="faberline/keep"
RELEASE_IMAGE_OWNER="ghcr.io/faberline"
RELEASE_COMPATIBILITY_LINE="- Compatibility: no HTTP, CLI, wire format, on-disk format, or Kubernetes manifest shape changed in this release."

release_refuse() { printf 'refused: %s\n' "$*" >&2; exit 2; }

release_app_known() {
  case "$1" in keep) return 0 ;; *) return 1 ;; esac
}
release_app_require() {
  release_app_known "${1:-}" || release_refuse "unknown release app: ${1:-<none>} (known: keep)"
}
# Where Cargo.toml, build.sh, and Dockerfile.release live.
release_app_root() {
  release_app_require "$1"
  printf '.\n'
}
# Directory of verify-release-candidate.sh and verify-release-artifacts.sh.
release_app_scripts_dir() {
  release_app_require "$1"
  printf 'scripts/release\n'
}
# True for the apps whose verifiers take --app.
release_app_uses_shared_scripts() {
  release_app_require "$1"
  return 0
}
release_app_targets() {
  release_app_require "$1"
  printf '%s\n' x86_64-unknown-linux-musl aarch64-unknown-linux-musl
}
# Which harness produces the GKE receipt.
release_app_gke_backend() {
  release_app_require "$1"
  printf 'gke-acceptance\n'
}
release_app_receipt_name() {
  release_app_require "$1"
  printf '%s-gke-receipt.json\n' "$1"
}
release_app_receipt_schema() {
  release_app_require "$1"
  printf '%s.gke-release-receipt/v1\n' "$1"
}
# Functional fields every receipt must carry as "passed".
release_app_functional_fields() {
  release_app_require "$1"
  printf '%s\n' readyz round_trip durability
}
# Promotion workflow input names.
release_app_promotion_input_prefix() {
  release_app_require "$1"
  printf 'gke_receipt\n'
}
release_app_promotion_takes_attempt() {
  release_app_require "$1"
  true
}
release_app_manifest_schema() {
  release_app_require "$1"
  printf 'cclab.%s.candidate-manifest.v1\n' "$1"
}
# Candidate job ids bound by the final manifest's `jobs` map.
release_app_candidate_jobs() {
  release_app_require "$1"
  printf '%s\n' identity build "$1-release-gates" ghcr-image-and-attest manifest verify-candidate result
}
# Display names of every job in one successful candidate attempt.
release_app_candidate_job_names() {
  release_app_require "$1"
  local target
  printf '%s\n' "candidate identity" "verify exact $1 release gates" "build candidate image and attest" "bind candidate inputs" "verify exact candidate gates" "final candidate receipt"
  while IFS= read -r target; do printf 'build (%s)\n' "$target"; done < <(release_app_targets "$1")
}

