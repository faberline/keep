//! Compatibility gate for the public surface (issue #104): the HTTP paths and
//! schema fields of `/openapi.json`, and the fields of the CRD, are compared
//! with committed snapshots. A removal fails the test; an addition fails it
//! too until the snapshot is regenerated on purpose.

use std::path::{Path, PathBuf};

const API_FIXTURE: &str = "tests/fixtures/api-surface.txt";
#[cfg(feature = "operator")]
const CRD_FIXTURE: &str = "tests/fixtures/crd-surface.txt";

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

const HTTP_METHODS: [&str; 7] = ["get", "put", "post", "delete", "patch", "head", "options"];

/// One line per HTTP method and path, and per schema and schema property.
fn surface_of_openapi(json: &str) -> Vec<String> {
    let doc: serde_json::Value = serde_json::from_str(json).expect("OpenAPI document is JSON");
    let mut lines = Vec::new();
    if let Some(paths) = doc.get("paths").and_then(|p| p.as_object()) {
        for (path, item) in paths {
            let Some(item) = item.as_object() else {
                continue;
            };
            for method in HTTP_METHODS {
                if item.contains_key(method) {
                    lines.push(format!("{} {path}", method.to_uppercase()));
                }
            }
        }
    }
    if let Some(schemas) = doc
        .pointer("/components/schemas")
        .and_then(|s| s.as_object())
    {
        for (name, schema) in schemas {
            lines.push(format!("schema {name}"));
            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                for prop in props.keys() {
                    lines.push(format!("schema {name}.{prop}"));
                }
            }
        }
    }
    lines.sort();
    lines.dedup();
    lines
}

/// One line per CRD property with its type, plus the served version.
#[cfg(feature = "operator")]
fn surface_of_crd(yaml: &str) -> Vec<String> {
    let doc: serde_json::Value = serde_yaml::from_str(yaml).expect("CRD is YAML");
    let version = &doc["spec"]["versions"][0];
    let mut lines = vec![format!(
        "version {}",
        version["name"].as_str().unwrap_or_default()
    )];
    let root = &version["schema"]["openAPIV3Schema"];
    collect_crd_properties(root, "", &mut lines);
    lines.sort();
    lines.dedup();
    lines
}

#[cfg(feature = "operator")]
fn collect_crd_properties(schema: &serde_json::Value, prefix: &str, lines: &mut Vec<String>) {
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    for (name, prop) in props {
        let dotted = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let ty = match prop.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None if prop.get("properties").is_some() => "object",
            None => "unknown",
        };
        lines.push(format!("{dotted}: {ty}"));
        collect_crd_properties(prop, &dotted, lines);
    }
}

/// The lines of `committed` that `current` no longer has.
fn removed_lines(committed: &str, current: &[String]) -> Vec<String> {
    committed
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| !current.iter().any(|c| c == line))
        .map(str::to_string)
        .collect()
}

fn check_fixture(path: &Path, current: &[String], what: &str) {
    if std::env::var("KEEP_UPDATE_SURFACE").as_deref() == Ok("1") {
        std::fs::write(path, current.join("\n") + "\n").expect("write fixture");
        return;
    }
    let committed = std::fs::read_to_string(path).expect("read committed fixture");
    let removed = removed_lines(&committed, current);
    if !removed.is_empty() {
        panic!("breaking change in {what}: removed {removed:?}. A removal needs a major release.");
    }
    let committed_lines: Vec<&str> = committed.lines().collect();
    let current_lines: Vec<&str> = current.iter().map(String::as_str).collect();
    if committed_lines != current_lines {
        panic!(
            "{what} changed (additive). Regenerate: KEEP_UPDATE_SURFACE=1 cargo test -p keep --test api_surface (add --features operator for the CRD)"
        );
    }
}

#[test]
fn api_surface_matches_committed_fixture() {
    check_fixture(
        &fixture(API_FIXTURE),
        &surface_of_openapi(&keep::spec::openapi_json()),
        "the OpenAPI document",
    );
}

#[cfg(feature = "operator")]
#[test]
fn crd_surface_matches_committed_fixture() {
    check_fixture(
        &fixture(CRD_FIXTURE),
        &surface_of_crd(&keep::operator::crd_yaml()),
        "the CRD",
    );
}

#[test]
fn removed_lines_reports_a_removed_path() {
    assert_eq!(
        removed_lines("GET /a\nGET /b\n", &["GET /a".to_string()]),
        vec!["GET /b".to_string()]
    );
    assert!(removed_lines("GET /a\n", &["GET /a".to_string(), "GET /c".to_string()]).is_empty());
}

#[test]
fn surface_of_openapi_lists_methods_and_properties() {
    let json = r#"{"paths":{"/x":{"get":{},"parameters":[]}},"components":{"schemas":{"S":{"properties":{"p":{}}}}}}"#;
    assert_eq!(
        surface_of_openapi(json),
        vec![
            "GET /x".to_string(),
            "schema S".to_string(),
            "schema S.p".to_string()
        ]
    );
}

#[cfg(feature = "operator")]
#[test]
fn surface_of_crd_lists_nested_properties() {
    let yaml = "\
spec:
  versions:
    - name: v1
      schema:
        openAPIV3Schema:
          properties:
            spec:
              properties:
                n:
                  type: integer
";
    // The lines are sorted with `sort()`, which orders "spec.n" before "spec:".
    let mut expected = vec![
        "spec: object".to_string(),
        "spec.n: integer".to_string(),
        "version v1".to_string(),
    ];
    expected.sort();
    assert_eq!(surface_of_crd(yaml), expected);
}
