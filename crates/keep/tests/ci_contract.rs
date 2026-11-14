//! The CI workflow (.github/workflows/ci.yml) is read as text: it must run
//! cargo audit in its own job, make the gate job wait for every test job, and
//! keep the audit config and the deny ignores in the reviewed shape.

const CI_WORKFLOW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/ci.yml"
);
const AUDIT_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.cargo/audit.toml");
const DENY_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deny.toml");

const DENY_IDS: [&str; 6] = [
    "RUSTSEC-2025-0012",
    "RUSTSEC-2024-0384",
    "RUSTSEC-2025-0141",
    "RUSTSEC-2024-0370",
    "RUSTSEC-2025-0134",
    "RUSTSEC-2024-0449",
];

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn workflow() -> String {
    read(CI_WORKFLOW)
}

/// The job ids of the `needs:` list of job `gate`.
fn gate_needs(text: &str) -> Vec<String> {
    let needs_line = text
        .lines()
        .skip_while(|l| *l != "  gate:")
        .find(|l| l.starts_with("    needs: ["))
        .unwrap_or_else(|| panic!("job gate has no `    needs: [` line"));
    let inner = needs_line
        .strip_prefix("    needs: [")
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or_else(|| panic!("malformed needs line: {needs_line}"));
    inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn ci_runs_cargo_audit() {
    let text = workflow();
    assert!(
        text.contains("cargo audit --deny unsound --deny yanked"),
        "ci.yml does not run `cargo audit --deny unsound --deny yanked`"
    );
    assert!(
        text.contains("cargo install --locked cargo-audit --version"),
        "ci.yml does not install a pinned cargo-audit"
    );
    assert!(
        text.lines().any(|l| l == "  test-audit:"),
        "ci.yml has no job `test-audit`"
    );
    assert!(
        gate_needs(&text).contains(&"test-audit".to_string()),
        "job gate does not need `test-audit`"
    );
}

#[test]
fn gate_needs_every_test_job() {
    let text = workflow();
    let needs = gate_needs(&text);
    assert!(
        needs.contains(&"lint".to_string()),
        "gate does not need lint"
    );
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("  test-") else {
            continue;
        };
        let Some(id) = rest.strip_suffix(':') else {
            continue;
        };
        let job = format!("test-{id}");
        assert!(
            needs.contains(&job),
            "job {job} is not in the needs of job gate"
        );
    }
}

#[test]
fn audit_config_has_no_ignores() {
    let text = read(AUDIT_CONFIG);
    assert!(
        text.contains("ignore = []"),
        ".cargo/audit.toml must keep `ignore = []`"
    );
}

#[test]
fn deny_ignores_name_their_path() {
    let text = read(DENY_CONFIG);
    let entries = text
        .lines()
        .filter(|l| l.contains("id = \"RUSTSEC-"))
        .count();
    assert_eq!(
        entries, 6,
        "deny.toml must keep exactly six RUSTSEC ignores"
    );
    for id in DENY_IDS {
        let needle = format!("id = \"{id}\"");
        let line = text
            .lines()
            .find(|l| l.contains(&needle))
            .unwrap_or_else(|| panic!("deny.toml has no ignore {id}"));
        assert!(
            line.contains("Path:") || line.contains("Build time only"),
            "the deny ignore {id} does not name its dependency path: {line}"
        );
    }
}
