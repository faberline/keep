# Contributing to keep

## Brief

How to change `keep`. What it promises and the work roots it owns live in
[README.md](README.md); repository-wide authoring and verification rules live
in the root [CONTRIBUTING.md](https://github.com/faberline/workspace/blob/main/CONTRIBUTING.md).

Work reaches `main` through the aw-worker flow of
[faberline/aw](https://github.com/faberline/aw). An issue moves through
three stages, shown by its label: `stage:spec` (write the spec),
`stage:tdd` (tests first, then code; the `aw` CLI opens the pull request)
and `stage:review` (review a pull request with green CI; `aw` merges it or
sends it back to tdd). The stage skill is
[aw-worker](https://github.com/faberline/aw/tree/main/plugins/faberline/skills/aw-worker);
the CI stages (`lint`, `test`, `e2e`, `gate`) are described in
[docs/standards](https://github.com/faberline/aw/tree/main/docs/standards).
The required check is `gate`. Only the `aw` CLI claims issues, pushes,
opens pull requests and merges.

## Verification

| Gate | Command |
|---|---|
| workspace unit + integration tests | `cargo test --workspace` |
| top-level package tests | `cargo test -p keep` |

