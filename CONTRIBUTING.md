# Contributing to keep

## Brief

How to change `keep`. What it promises and the work roots it owns live in
[README.md](README.md); repository-wide authoring and verification rules live
in [AW standards](https://github.com/faberline/aw/tree/main/docs/standards).

Use the shared Faberline `issue-plan`, `issue-work`, and `issue-review`
skills from [AW](https://github.com/faberline/aw/tree/main/plugins/faberline/skills).
Each skill runs one pass; its `-loop` companion adds an explicit interval.
Installing or updating a skill does not start a loop.

Planning writes the bounded issue contract. Work submits one issue's PR and
runs affected unit/static checks. CI runs the required functional, integration
and performance checks. It alone sets `done` after all required current-input
gates pass, leaving the issue open for independent local review. A required
CI failure reports its evidence and returns the issue to `todo`. Review reads
the issue, diff and current CI; it runs no tests and never merges, closes or
changes lifecycle labels. The controller owns Git and final acceptance.

Use the repository's existing complete gates for product claims. Keep local
work limited to affected unit/static checks; functional, integration and
performance execution belongs in authorized CI. Preserve all feature matrices,
test selection checks, numeric limits and exact-head/base evidence.

## Verification

| Gate | Command |
|---|---|
| workspace unit + integration tests | `cargo test --workspace` |
| top-level package tests | `cargo test -p keep` |

