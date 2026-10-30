# Ordinary PR unit and static checks

This candidate continues the human-requested common coverage work after
[Core #74](https://github.com/faberline/core/pull/74),
[Lumen #185](https://github.com/faberline/lumen/pull/185) and
[Tape #50](https://github.com/faberline/tape/pull/50).
The baseline is `fd71d0942d2dcef73b34f25a72e590a91b7aee55`.

Keep already runs workspace formatting, all-target/all-feature Clippy,
default-feature workspace tests and the release contract. Those jobs remain.
The additional workflow runs every library and binary unit target separately
for each declared feature profile. It uses Rust **1.92**, matching the existing
Keep CI and Dockerfile. The copied planner does not approve qualification.

## Complete manifest-derived plan

The locked metadata inventory contains nine packages and ten unit targets:

| Package | Unit targets | Feature rows |
|---|---:|---:|
| `keep` | 2 | 10 |
| `keep-access` | 1 | 1 |
| `keep-client` | 1 | 1 |
| `keep-commands` | 1 | 4 |
| `keep-keyspace` | 1 | 1 |
| `keep-operator` | 1 | 1 |
| `keep-replication` | 1 | 4 |
| `keep-shared-kernel` | 1 | 4 |
| `keep-storage` | 1 | 1 |

Each package has a default row. Feature-bearing packages also have no-default,
one row per named feature and all-features. The deprecated `report-issue` alias
retains its own row. These profiles are not the feature power set; existing
combined-feature contracts keep their own checks. Optional local dependencies
remain in the advisory reverse-dependency graph.

The baseline plan has **27 rows, 37 unit invocations and 27 all-target Clippy
invocations**. These counts describe intended execution. They do not prove
that any product test passed. Release executions for ignored tests can add
invocations. The checked-in JSON files are inspection snapshots. Each cloud
job regenerates metadata from its exact full head and base identities.

Every PR runs the complete workspace, including docs-only changes. Advisory
affected-path selection cannot remove execution. Missing or duplicate packages,
disabled harnesses, lost baseline targets/features and matrices over 256 rows
fail instead of being accepted or truncated.
Optional dependency features also select binaries whose required features they
enable. Weak dependency features do not enable a dependency on their own.

## Passing evidence

Each enabled target must produce a complete, named, nonempty passing roster.
Zero, failed, filtered, measured, duplicate, incomplete or missing results fail.
This candidate grants no empty-target exception. A failed target does not hide
later targets. Clippy still runs unless the workflow is cancelled.
Each roster must match the harness discovery list before and after execution.
The planner checks both checkouts for tracked and non-ignored untracked changes
before and after metadata generation and product commands. A detected change
fails the job. This does not prove that files cannot change and be restored
during a run.

If debug reports ignored tests, the entire same target must also pass in
release with `--include-ignored`. Every ignored name must pass there. Both
executions must succeed; the release roster must have zero failures, ignored,
measured or filtered tests and match its discovery lists. Every debug test name
must remain in the release list. Existing performance assertions stay unchanged.

Clippy uses `--all-targets -- -D warnings` in each feature row. Formatting is
independent of the matrix. The workflow uses pinned actions, read-only
permissions and no persisted checkout credential. At most four rows run at
once. The 30-minute job budget is not a product-performance oracle. Dependency
caches exclude workspace source crates.

## Preparation checkpoint, 2026-10-29

`scripts/pr-gates/plan.py` and its 53 controls are byte-identical to
`faberline/core@140b30f550d974d088f9b2f018c2b2ca3ff423e2`. That review does
not independently review this Keep adoption. The isolated controls passed
locally with Python 3.13. Locked offline metadata was inspected without a
product build. Local metadata used Cargo 1.99; product CI remains on Rust 1.92.
Rust product tests, builds and Clippy were not run locally.

```sh
cargo metadata --locked --no-deps --offline --format-version 1 > /tmp/keep-metadata.json
python3.13 scripts/pr-gates/plan.py inventory --root . --metadata-json /tmp/keep-metadata.json
python3.13 scripts/pr-gates/plan.py matrix --root . --metadata-json /tmp/keep-metadata.json
python3.13 -m unittest discover -s scripts/pr-gates/tests -v
```

Online execution and independent adoption review remain required. Candidate
code execution does not establish trusted qualification. The live failure-only
policy retains both blockers: GKE pilot PR #6 is unmerged, and an approved
affected performance gate is missing. This proposal does not activate those
policies, alter the existing CI/release contract, modify a product PR, merge,
release or change another session's schedule.
