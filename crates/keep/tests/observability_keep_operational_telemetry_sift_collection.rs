// SPEC-MANAGED: external-contracts/operational-telemetry/observability/sift-data-collection.md#keep-operational-telemetry-sift-collection
// CODEGEN-BEGIN
// AW-EC-BEGIN
// @ec keep-operational-telemetry-sift-collection
// @capability operational-telemetry
// @claim jsonl-stdout-collector-contract
// @contract keep-axiom-service-log-v1
// @category observability
// @required_for_production false
// @command cargo test --locked -p keep --test structured_stdout_traceparent --test operator_telemetry
// AW-EC-END

// Contract: A real keep process with --log-format json emits at least one stdout record, and every record validates against core's axiom.service.log.v1 schema with service.name=keep.
// Contract: The request record for a call carrying a W3C traceparent reports that trace id and parent span; a call with an invalid or missing traceparent gets a fresh, distinct, valid trace id.
// Contract: The schema validator rejects malformed records (missing service, bad severity, bad timestamp, bad trace id, unknown or nested fields, empty event).
// Contract: Operator-rendered and k8s/base pods both start with KEEP_LOG_FORMAT=json and KEEP_BODY_LIMIT projected from the keep ConfigMap, and neither carries Sift collector configuration.
// Contract: The wrapper fails closed when the selected tests report zero passes.
#[test]
#[ignore = "AW EC gate: run via `aw health --verify-ec` or `cargo test -- --ignored`"]
fn keep_operational_telemetry_sift_collection() {
    let command = "cargo test --locked -p keep --test structured_stdout_traceparent --test operator_telemetry";
    let id = "keep-operational-telemetry-sift-collection";
    let mut root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !root.join(".aw").is_dir() {
        assert!(
            root.pop(),
            "AW EC {id}: no .aw/ project root above {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&root)
        .output()
        .unwrap_or_else(|e| panic!("AW EC {id}: failed to spawn `{command}`: {e}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "AW EC {id} FAILED (exit {:?}): {command}\n{stdout}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    // Fail closed: a renamed or filtered-out test must not pass as zero tests.
    for test in [
        "real_keep_process_correlates_structured_stdout",
        "schema_validator_rejects_malformed_records",
        "operator_renders_jsonl_logging_and_body_limit",
        "base_manifests_match_operator_telemetry_env",
    ] {
        assert!(
            stdout.contains(&format!("test {test} ... ok")),
            "AW EC {id}: `{test}` did not run and pass\n{stdout}"
        );
    }
}
// CODEGEN-END
