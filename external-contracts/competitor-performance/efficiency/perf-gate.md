---
id: keep-competitor-performance-ec
summary: Keep performance evidence is meter-owned and vat-isolated; keep-vs-Redis/Dragonfly ratios are a measured ratchet (KeyDB/Garnet report-only).
fill_sections: [e2e-test, tool-contract]
---

# EC: Competitive KV Performance

Keep's production performance evidence is the meter-owned engine/runtime gate.
The current vat runner delegates through meter and keeps the transient benchmark
workspace isolated.

Competitive standing is a separate ratchet: keep is measured against pinned
Redis, Dragonfly, KeyDB, and Garnet containers on the same host, and the
keep/peer ratios must not fall below the baseline recorded for that host class.
Redis and Dragonfly cells gate; KeyDB and Garnet are reported only. It runs as
release-only nightly / on-demand evidence, not on every PR.

Gate commands, thresholds, and reference baselines are in
[`meter-performance-gates.md`](meter-performance-gates.md) (meter) and
[`competitor-ratchet.md`](competitor-ratchet.md) (competitors).

## External Contract
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: keep-competitor-performance-meter-gate
    capability_id: competitor-performance
    claim_id: vat-meter-runtime-gate
    contract_id: keep-meter-performance-report
    category: efficiency
    test_path: crates/keep/tests/benchmark_keep_competitor_performance_meter_gate.rs
    command: "vat run meter-efficiency"
    assertions:
      - "meter owns the pass/fail evidence for Keep's performance-relevant API and engine gate."
      - "The gate runs inside vat so report artifacts and transient state do not mutate the host checkout."
      - "Redis/Dragonfly comparison remains dogfood until external peer services are required by the EC."
  - id: keep-competitor-performance-ratchet
    capability_id: competitor-performance
    claim_id: competitor-ratio-ratchet
    contract_id: keep-competitor-ratio-baseline
    category: efficiency
    test_path: crates/keep/tests/benchmark_keep_competitor_performance_ratchet.rs
    command: "vat run perf-gate-competitors"
    assertions:
      - "keep/peer throughput and p99 ratios for SET, GET, and CAS at 1 and 4 server cores, RAM and durable tiers, are judged against crates/keep/tests/perf-baseline.json for the host class."
      - "A Redis or Dragonfly cell fails when its ratio drops below max(min_ratio, 0.85 x baseline) for throughput or 0.70 x baseline for p99; KeyDB and Garnet cells are report-only."
      - "Durable-tier peers run fsync-per-write AOF and every server's storage mode is verified before its numbers count."
      - "The gate fails closed on a missing host baseline, an unmeasured ratchet cell, an emulated image, an unready server, or any failed request."
```

## Tool Contract
<!-- type: tool-contract lang: yaml -->

```yaml
tool_contracts:
  - id: keep-meter-performance
    tool: meter
    manifest: meter-keep-performance.toml
    category: efficiency
    command: "vat run meter-efficiency"
    native:
      version: 1
      project: keep
      source_contract: keep-competitor-performance-meter-gate
      delegate_command: "vat run meter-efficiency"
```
