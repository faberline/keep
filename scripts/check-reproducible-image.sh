#!/usr/bin/env bash
# Build Dockerfile.release twice from the staged binary (dist/linux/amd64/keep)
# and fail when the two OCI layouts differ. Run from the repository root.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
[[ -f dist/linux/amd64/keep ]] || { echo "missing dist/linux/amd64/keep (stage the candidate binary first)" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

build() {
  docker buildx build \
    --no-cache \
    --platform linux/amd64 \
    --provenance=false \
    --sbom=false \
    --file Dockerfile.release \
    --build-arg SOURCE=staged \
    --build-arg SOURCE_DATE_EPOCH=0 \
    --output "type=oci,dest=$1,rewrite-timestamp=true" \
    .
}

digest_of() {
  tar -xOf "$1" index.json | sha256sum | cut -d' ' -f1
}

build "$tmp/a.tar"
build "$tmp/b.tar"
a="$(digest_of "$tmp/a.tar")"
b="$(digest_of "$tmp/b.tar")"
if [[ "$a" != "$b" ]]; then
  echo "image is not reproducible" >&2
  echo "build a: sha256:$a" >&2
  echo "build b: sha256:$b" >&2
  exit 1
fi
echo "reproducible: sha256:$a"
