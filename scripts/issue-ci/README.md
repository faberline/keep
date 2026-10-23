# CI lifecycle handoff

Vendored without modification from `faberline/aw@67af8d020df3ca2ec4b064289408cfb0c2f8b299`
`plugins/faberline/skills/issue-ci/scripts/handoff.py`. SHA-256:
`c67533b39d45ad62d407af3748856be396941647c349b63464424fa6aff1c198`.

The exact existing required gate is `keep-ci` (`.github/workflows/keep-ci.yml`):
`fmt, clippy, tests, release contract`. This explicit manifest is the reviewed
policy source; visible green checks and branch protection are not inferred.

## Failure-only rollout

The guard imports the pinned shared controller's `load_config` and requires
its `evaluate_policy` API, explicit `lifecycle_mode: "failure-only"`, and a
nonempty `qualification_blockers` list. Old controllers, full mode, malformed
or missing policy, and clearing blockers refuse before the privileged call.
Confirmed current-head failures can return doing issues to todo with links.
Success, missing, skipped or pending evidence never writes done; already
done issues remain untouched. All gates and coverage gaps remain explicit.
The live gates above alone do not prove the approved MR policy (affected
unit, functional, e2e, performance).

- No merged current-head cloud functional/e2e qualification gate; GKE pilot PR #6 is unmerged.
- No approved affected performance qualification gate. Existing unit/integration and release contract checks do not prove full MR qualification.

Resolve these decisions in a separate reviewed change that supplies real
qualification and updates the explicit gate contract before clearing blockers
or enabling full mode. The controller itself suppresses done for any nonempty
blockers even if a future caller accidentally selects full mode.
No new cloud jobs, GKE triggers, performance runs, or credentials are added.
No docs-only exemption or zero-check pass is assumed.

AGY owns only todo → doing. CI aggregates every required workflow and job
for the current PR head; failure returns todo with run/job links. This
failure-only rollout never produces done. After separately approved full
qualification, review independently verifies the same head after success
and does not merge. Managed PRs must use
`issue/N` or `agy/issue-N` and include `Closes #N`. Unrelated labels remain.

The serialized controller reconciles all managed PRs on every wake, reads
only GitHub metadata, and uses latest run attempts. Missing, skipped, stale,
neutral, cancelled, or pending evidence cannot pass. It checks the head
before writes and journals handoffs for deduplication and recovery. Checkout
is the trusted default branch; PR code and artifacts are never executed or
used as permission evidence. The handoff workflow becomes effective only
after merge; this PR does not alter automation schedules.

Local validation: `python3 -m unittest discover -s scripts/issue-ci/tests -v`.
