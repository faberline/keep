# Keep alert index

This repository has no alert rules. This page names the signals an operator may
alert on, and for each one points to the runbook section with the check and the
action. It creates no alert rules. Its links are checked by
`crates/keep/tests/docs_lint.rs`. The upgrade checks are in
[runbook.md#upgrade](runbook.md#upgrade). `curl` and `kubectl` lines on this page
and in the linked sections are not tested.

## Node down

Symptom: the pod is not Ready, and `GET /healthz` fails.
Check: [Node down](runbook.md#node-down)
Action: [Node down](runbook.md#node-down)

## Disk full

Symptom: `GET /info` field `disk_blob_bytes` is near `--max-disk-bytes`.
Check: [Disk full](runbook.md#disk-full)
Action: [Disk full](runbook.md#disk-full)

## Replica behind

Symptom: `applied_index` on a replica is below the leader's `commit_index`.
Check: [Replica behind](runbook.md#replica-behind)
Action: [Replica behind](runbook.md#replica-behind)

## Readiness failing

Symptom: `GET /readyz` does not return `200`.
Check: [Readiness failing](runbook.md#readiness-failing)
Action: [Readiness failing](runbook.md#readiness-failing)

## Token rejects

Symptom: HTTP `401` or `403` on a client request.
Check: [Token rejects](runbook.md#token-rejects)
Action: [Token rotation](runbook.md#token-rotation)
