# AGENTS.md

keep is a cloud-native, multi-core key-value and claim-check store: a Redis / Dragonfly replacement with an HTTP/2 + OpenAPI API.

## Output style

- Write in the user's language.
- Keep paths, commands and exact output intact.
- Report what changed, what passed, what is unknown and the next action.

## Scope

- This repository is public. Do not put a secret, token, private host name, e-mail address or personal path into any file, issue or comment.
- What must stay true: the promises in `README.md`. A promise with no gate under it is not claimed.
- Work roots and the capability index live in `README.md`. How to change keep lives in `CONTRIBUTING.md`.

## Issue work

- Work on a claimed issue only through the `aw` CLI (`aw claim`, the work, `aw report`). `aw` does the branch, push, pull request, merge and signal comment.
- Edit labels only in the plan step of the `aw-worker` skill.
- The required check is `gate` (see `CONTRIBUTING.md`).

## Verify

Run these commands from the repository root. Each one is a `run:` command of `.github/workflows/ci.yml`:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- `cargo test --locked --workspace --doc`
- `cargo deny --locked check advisories licenses bans sources`
- `cargo audit --deny unsound --deny yanked`

CI also runs `cargo llvm-cov --workspace --locked --json --summary-only --output-path target/coverage.json` and feature-gated `cargo test --locked -p keep --features raft --test it <module>::` runs; see `.github/workflows/ci.yml`.

Do not delete, skip or weaken a test, a threshold or the coverage setting.
