// SPEC-MANAGED: external-contracts/competitor-performance/efficiency/perf-gate.md#keep-competitor-performance-ratchet
// CODEGEN-BEGIN
// AW-EC-BEGIN
// @ec keep-competitor-performance-ratchet
// @capability competitor-performance
// @claim competitor-ratio-ratchet
// @contract keep-competitor-ratio-baseline
// @category efficiency
// @required_for_production false
// @command vat run perf-gate-competitors
// AW-EC-END

// Contract: keep/peer throughput and p99 ratios for SET, GET, and CAS at 1 and 4 server cores, RAM and durable tiers, are judged against crates/keep/tests/perf-baseline.json for the host class.
// Contract: A Redis or Dragonfly cell fails when its ratio drops below max(min_ratio, 0.85 x baseline) for throughput or 0.70 x baseline for p99; KeyDB and Garnet cells are report-only.
// Contract: Durable-tier peers run fsync-per-write AOF and every server's storage mode is verified before its numbers count.
// Contract: The gate fails closed on a missing host baseline, an unmeasured ratchet cell, an emulated image, an unready server, or any failed request.
#[test]
#[ignore = "AW EC gate: run via `aw health --verify-ec` or `cargo test -- --ignored`"]
fn keep_competitor_performance_ratchet() {
    let command = "vat run perf-gate-competitors";
    let id = "keep-competitor-performance-ratchet";
    let mut root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !root.join(".aw").is_dir() {
        assert!(
            root.pop(),
            "AW EC {id}: no .aw/ project root above {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&root)
        .status()
        .unwrap_or_else(|e| panic!("AW EC {id}: failed to spawn `{command}`: {e}"));
    assert!(
        status.success(),
        "AW EC {id} FAILED (exit {:?}): {command}",
        status.code()
    );
}
// CODEGEN-END
