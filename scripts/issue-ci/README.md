# CI lifecycle handoff

Vendored without modification from `faberline/aw@84e9b72b4a63cc30b47449a2e321981525fa9c34`
`plugins/faberline/skills/issue-ci/scripts/handoff.py`. SHA-256:
`1abb5570032eaf294a50c5fa0bfb9ae7edd1db7e3f0a98a1f56601720916caf1`.

The exact existing required gate is `keep-ci` (`.github/workflows/keep-ci.yml`):
`fmt, clippy, tests, release contract`. This explicit manifest is the reviewed
policy source; visible green checks and branch protection are not inferred.

## Rollout is blocked

The workflow refuses all lifecycle mutations while `qualification_blockers`
is nonempty or missing. The live gates above alone do not prove the approved
MR policy (affected unit, functional, e2e, performance).

- No merged current-head cloud functional/e2e qualification gate; GKE pilot PR #6 is unmerged.
- No approved affected performance qualification gate. Existing unit/integration and release contract checks do not prove full MR qualification.

Resolve these decisions in a separate reviewed change that supplies real
qualification and updates the explicit gate contract before clearing blockers.
No new cloud jobs, GKE triggers, performance runs, or credentials are added.
No docs-only exemption or zero-check pass is assumed.

After activation, AGY owns only todo → doing. CI aggregates every required
workflow and job for the current PR head; failure returns todo with run/job
links and only complete success gives done. Review independently verifies
the same current head after success and does not merge. Managed PRs must use
`issue/N` or `agy/issue-N` and include `Closes #N`. Unrelated labels remain.

The serialized controller reconciles all managed PRs on every wake, reads
only GitHub metadata, and uses latest run attempts. Missing, skipped, stale,
neutral, cancelled, or pending evidence cannot pass. It checks the head
before writes and journals handoffs for deduplication and recovery. Checkout
is the trusted default branch; PR code and artifacts are never executed or
used as permission evidence. The handoff workflow becomes effective only
after merge; this PR does not alter automation schedules.

Local validation: `python3 -m unittest discover -s scripts/issue-ci/tests -v`.
