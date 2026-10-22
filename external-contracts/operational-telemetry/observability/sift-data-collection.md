---
id: keep-operational-telemetry-ec
summary: Keep emits axiom.service.log.v1 JSONL on stdout with W3C trace correlation, so a deployment-owned collector (Sift) can ingest it without a keep-side Sift dependency.
fill_sections: [e2e-test]
---

# EC: Operational Telemetry — Sift Data Collection

Keep's collector contract is its process stdout. A real `keep` process started
with `--log-format json` writes one `axiom.service.log.v1` record per line, and
request records carry the caller's W3C `traceparent` trace id. The schema is the
one faberline/core checks in (`service_observability::service_log_schema_v1()`),
so keep cannot drift from what the collector validates.

Collector routing, retention, and the Sift pipeline are deployment-owned: keep
ships no Sift client, endpoint, or credential, and its Kubernetes manifests
(operator render and `k8s/base`) only switch stdout to JSONL. The cross-service
Sift VAT journey that ingests these records lives in faberline/sift (#18).

## External Contract
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: keep-operational-telemetry-sift-collection
    capability_id: operational-telemetry
    claim_id: jsonl-stdout-collector-contract
    contract_id: keep-axiom-service-log-v1
    category: observability
    test_path: crates/keep/tests/observability_keep_operational_telemetry_sift_collection.rs
    command: "cargo test --locked -p keep --test structured_stdout_traceparent --test operator_telemetry"
    required_for_production: false
    assertions:
      - "A real keep process with --log-format json emits at least one stdout record, and every record validates against core's axiom.service.log.v1 schema with service.name=keep."
      - "The request record for a call carrying a W3C traceparent reports that trace id and parent span; a call with an invalid or missing traceparent gets a fresh, distinct, valid trace id."
      - "The schema validator rejects malformed records (missing service, bad severity, bad timestamp, bad trace id, unknown or nested fields, empty event)."
      - "Operator-rendered and k8s/base pods both start with KEEP_LOG_FORMAT=json and KEEP_BODY_LIMIT projected from the keep ConfigMap, and neither carries Sift collector configuration."
      - "The wrapper fails closed when the selected tests report zero passes."
```
