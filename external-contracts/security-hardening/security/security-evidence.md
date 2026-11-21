---
id: keep-security-hardening-ec
summary: Keep security evidence is the vat-isolated runtime suites; the static check is aw security check.
fill_sections: [e2e-test]
---

# EC: Security Hardening

Keep's security gate is the runtime security suites over the public HTTP and TLS surface, run inside vat. The static check is aw security check (standard S10); it is not part of this gate.

## External Contract
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: keep-security-hardening-runtime
    capability_id: security-hardening
    claim_id: runtime-security-evidence
    contract_id: keep-runtime-security-report
    category: security
    test_path: crates/keep/tests/it/security_keep_security_hardening_runtime.rs
    command: "vat run security-runtime"
    required_for_production: false
    assertions:
      - "The runtime security suites (public HTTP route smoke and public API TLS) pass and run a nonzero number of tests."
      - "The suites run inside vat so generated reports and transient files do not mutate the host checkout."
      - "No guard scan is required; aw security check is the static security check."
```
