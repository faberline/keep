// Keep stays Sift-agnostic (issue #4, slice a): the stdout contract checker
// rejects bad records, and no Keep Cargo file, Rust source or k8s manifest
// depends on or configures Sift outside comments.

use std::path::{Path, PathBuf};

use serde_json::json;

fn parse_service_log_records(lines: &[String]) -> Result<Vec<serde_json::Value>, String> {
    let mut records = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: serde_json::Value =
            serde_json::from_str(trimmed).map_err(|e| format!("not JSON: {trimmed:?}: {e}"))?;
        if record["schema"] != "axiom.service.log.v1" {
            return Err(format!(
                "wrong schema: expected axiom.service.log.v1, got {}",
                record["schema"]
            ));
        }
        if record["service"]["name"] != "keep" {
            return Err(format!(
                "wrong service.name: expected keep, got {}",
                record["service"]["name"]
            ));
        }
        records.push(record);
    }
    if records.is_empty() {
        return Err("no records in stdout".to_string());
    }
    Ok(records)
}

fn sift_references(text: &str, comment_prefix: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with(comment_prefix))
        .filter(|line| line.to_lowercase().contains("sift"))
        .map(str::to_string)
        .collect()
}

fn collect_files(dir: &Path, extensions: &[&str], out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name != "target" && name != "tests" {
                collect_files(&path, extensions, out);
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e))
        {
            out.push(path);
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn parse_accepts_valid_records() {
    let line = json!({
        "schema": "axiom.service.log.v1",
        "service": {"name": "keep"},
        "message": "a"
    })
    .to_string();
    let lines = vec![line.clone(), String::new(), line];
    let records = parse_service_log_records(&lines).expect("valid keep records are accepted");
    assert_eq!(records.len(), 2, "two records expected, got {records:?}");
}

#[test]
fn parse_rejects_empty_stdout() {
    let err = parse_service_log_records(&[]).expect_err("empty stdout must be rejected");
    assert!(err.contains("no records"), "got: {err}");

    let blank = vec![String::new(), "  ".to_string()];
    let err = parse_service_log_records(&blank).expect_err("blank-only stdout must be rejected");
    assert!(err.contains("no records"), "got: {err}");
}

#[test]
fn parse_rejects_non_json_line() {
    let lines = vec!["not json".to_string()];
    let err = parse_service_log_records(&lines).expect_err("a non-JSON line must be rejected");
    assert!(err.contains("not JSON"), "got: {err}");
}

#[test]
fn parse_rejects_wrong_schema() {
    let lines = vec![json!({
        "schema": "other.v1",
        "service": {"name": "keep"}
    })
    .to_string()];
    let err = parse_service_log_records(&lines).expect_err("a wrong schema must be rejected");
    assert!(err.contains("schema"), "got: {err}");
}

#[test]
fn parse_rejects_wrong_service_name() {
    let lines = vec![json!({
        "schema": "axiom.service.log.v1",
        "service": {"name": "sift"}
    })
    .to_string()];
    let err = parse_service_log_records(&lines).expect_err("a wrong service.name must be rejected");
    assert!(err.contains("service.name"), "got: {err}");
}

#[test]
fn sift_scan_finds_code_and_skips_comments() {
    let text = "// Sift routing is deployment-owned\nlet u = \"SIFT_URL\";\n";
    let hits = sift_references(text, "//");
    assert_eq!(
        hits,
        vec!["let u = \"SIFT_URL\";".to_string()],
        "only the code line may match; the comment must be skipped"
    );
}

#[test]
fn keep_workspace_has_no_sift_dependency_or_config() {
    let root = workspace_root();

    let mut tomls = Vec::new();
    collect_files(&root.join("crates"), &["toml"], &mut tomls);
    let mut rs = Vec::new();
    collect_files(&root.join("crates"), &["rs"], &mut rs);
    let mut yaml = Vec::new();
    collect_files(&root.join("k8s"), &["yaml", "yml"], &mut yaml);

    let mut scanned: Vec<(PathBuf, &str)> = vec![(root.join("Cargo.toml"), "#")];
    for path in tomls {
        if path.file_name().is_some_and(|name| name == "Cargo.toml") {
            scanned.push((path, "#"));
        }
    }
    for path in rs {
        scanned.push((path, "//"));
    }
    for path in yaml {
        scanned.push((path, "#"));
    }

    assert!(
        scanned.len() >= 10,
        "scanned only {} files; the walk must fail closed",
        scanned.len()
    );

    let mut hits = Vec::new();
    for (path, prefix) in &scanned {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for line in sift_references(&text, prefix) {
            hits.push(format!("{}: {}", path.display(), line.trim()));
        }
    }
    assert!(
        hits.is_empty(),
        "Keep must not depend on or configure Sift outside comments:\n{}",
        hits.join("\n")
    );
}
