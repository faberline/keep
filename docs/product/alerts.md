# Keep alert index

This repository has no alert rules. This page names the failure signals an
operator may alert on, and points each one to the runbook section that holds
its check and its action. It creates no alert rules. The runbook links here
are checked by `crates/keep/tests/docs_lint.rs`. For the upgrade checks, see
[Upgrade](runbook.md#upgrade). The `curl` and `kubectl` lines on this page and
in the linked sections are not tested.

## Node down

Symptom: the pod is not Ready, and `GET /healthz` fails.

Check: [Node down](runbook.md#node-down)

Action: [Node down](runbook.md#node-down)

## Disk full

Symptom: the metric `keep_disk_blob_bytes` is near `--max-disk-bytes`.

Check: [Disk full](runbook.md#disk-full)

Action: [Disk full](runbook.md#disk-full)

## Replica behind

Symptom: a replica's `applied_index` is below the leader's `commit_index`.

Check: [Replica behind](runbook.md#replica-behind)

Action: [Replica behind](runbook.md#replica-behind)

## Readiness failing

Symptom: `GET /readyz` is not `200`.

Check: [Readiness failing](runbook.md#readiness-failing)

Action: [Readiness failing](runbook.md#readiness-failing)

## Token rejects

Symptom: HTTP `401` or `403` responses from the API.

Check: [Token rejects](runbook.md#token-rejects)

Action: [Token rejects](runbook.md#token-rejects)
