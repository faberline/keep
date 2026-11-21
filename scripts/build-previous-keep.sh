#!/usr/bin/env bash
# Build keep at the baseline commit named in scripts/previous-keep-sha and copy
# the binary to target/previous/keep, for the rolling upgrade tests.
# Run from the repository root. The clone needs full history (fetch-depth: 0),
# or the commit is fetched from origin when it is missing.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
[[ -f scripts/previous-keep-sha ]] || { echo "missing scripts/previous-keep-sha" >&2; exit 2; }

sha="$(tr -d '[:space:]' < scripts/previous-keep-sha)"
if ! git cat-file -e "$sha^{commit}" 2>/dev/null; then
  git fetch --no-tags origin "$sha" >&2
fi

work="$(mktemp -d)"
trap 'git worktree remove --force "$work/src" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

git worktree add --detach "$work/src" "$sha" >&2
(
  cd "$work/src"
  cargo build --locked -p keep --features raft --bin keep --target-dir "$root/target/previous-build"
)

mkdir -p target/previous
cp target/previous-build/debug/keep target/previous/keep
echo "built $sha to target/previous/keep"
