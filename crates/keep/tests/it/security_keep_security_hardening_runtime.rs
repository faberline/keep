// SPEC-MANAGED: external-contracts/security-hardening/security/security-evidence.md#keep-security-hardening-runtime
// CODEGEN-BEGIN
// AW-EC-BEGIN
// @ec keep-security-hardening-runtime
// @capability security-hardening
// @claim runtime-security-evidence
// @contract keep-runtime-security-report
// @category security
// @required_for_production false
// @command vat run security-runtime
// AW-EC-END

// Contract: runtime security suites (public HTTP route smoke and public API TLS) pass with a nonzero test count.
// Contract: The suites run inside vat so reports and transient files do not mutate the host checkout.
// Contract: No guard scan is required; the static check is aw security check (standard S10).
#[test]
#[ignore = "AW EC gate: run via `aw health --verify-ec` or `cargo test -- --ignored`"]
fn keep_security_hardening_runtime() {
    let command = "vat run security-runtime";
    let id = "keep-security-hardening-runtime";
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
