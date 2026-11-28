//! S4 core-reuse rules as tests (CR1, CR2, CR4). Every `faberline/core`
//! dependency pins one stable tag, and each member crate takes the core
//! crates from the workspace. The manifests are read as text, so this runs
//! offline and needs no TOML parser.

use std::path::Path;

const CORE_URL: &str = "https://github.com/faberline/core";

/// The core release that keep pins. A re-pin changes this and the manifests together.
const CORE_PIN: &str = "v0.5.5";

const MEMBERS: [&str; 9] = [
    "keep",
    "keep-access",
    "keep-client",
    "keep-commands",
    "keep-keyspace",
    "keep-operator",
    "keep-replication",
    "keep-shared-kernel",
    "keep-storage",
];

/// The text of a repository file, read from the repository root.
fn read(rel: &str) -> String {
    let full = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../")).join(rel);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|error| panic!("read {}: {error}", full.display()))
}

/// True for exactly `vX.Y.Z` with digits only, so `-rc.1` and similar fail.
fn is_stable_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// The tag of each core dependency line in `manifest`. A line that names a
/// `path`, `branch` or `rev` instead of a stable tag is an error (CR1).
fn core_tags(manifest: &str) -> Result<Vec<String>, String> {
    const TAG_KEY: &str = "tag = \"";
    let mut tags = Vec::new();
    for line in manifest.lines().filter(|line| line.contains(CORE_URL)) {
        if line.contains("path =") || line.contains("branch =") || line.contains("rev =") {
            return Err(format!("core dependency must pin a tag only: {line}"));
        }
        let Some(start) = line.find(TAG_KEY) else {
            return Err(format!("core dependency has no tag: {line}"));
        };
        let after = &line[start + TAG_KEY.len()..];
        let tag = after.split('"').next().unwrap_or_default();
        if !is_stable_tag(tag) {
            return Err(format!("tag is not a stable vX.Y.Z: {tag}"));
        }
        tags.push(tag.to_string());
    }
    Ok(tags)
}

/// The single core tag that every dependency uses (CR2). It names no version.
fn one_tag(tags: &[String]) -> Result<String, String> {
    let Some(first) = tags.first() else {
        return Err("no faberline/core dependency".to_string());
    };
    if tags.iter().any(|tag| tag != first) {
        return Err(format!("more than one core tag: {tags:?}"));
    }
    Ok(first.clone())
}

/// Lines of a member manifest that take a core crate directly, with a git
/// source or a version instead of `workspace = true` (CR4). Only dependency
/// tables are read, except that a line with the core URL is always flagged.
fn member_core_violations(manifest: &str, core_crates: &[String]) -> Vec<String> {
    let mut violations = Vec::new();
    let mut in_dependencies = false;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_dependencies = line.ends_with("dependencies]");
            continue;
        }
        if line.contains(CORE_URL) {
            violations.push(line.to_string());
            continue;
        }
        if !in_dependencies || line.contains("workspace = true") {
            continue;
        }
        for name in core_crates {
            if line.starts_with(&format!("{name} =")) || line.starts_with(&format!("{name}.")) {
                violations.push(line.to_string());
            }
        }
    }
    violations
}

/// The dependency names of the core lines in the workspace manifest.
fn core_crate_names(workspace_manifest: &str) -> Vec<String> {
    workspace_manifest
        .lines()
        .filter(|line| line.contains(CORE_URL))
        .filter_map(|line| line.split(" =").next())
        .map(|name| name.trim().to_string())
        .collect()
}

#[test]
fn core_pin_is_one_stable_tag() {
    let tags = core_tags(&read("Cargo.toml")).unwrap_or_else(|error| panic!("{error}"));
    let tag = one_tag(&tags).unwrap_or_else(|error| panic!("{error}"));
    assert!(
        tag.starts_with('v') && !tag.contains("rc"),
        "core tag {tag} is not a stable release"
    );
}

#[test]
fn core_deps_use_workspace_inheritance() {
    let names = core_crate_names(&read("Cargo.toml"));
    assert!(
        names.len() >= 10,
        "the root Cargo.toml names only {} core crates: {names:?}",
        names.len()
    );
    for member in MEMBERS {
        let manifest = read(&format!("crates/{member}/Cargo.toml"));
        let violations = member_core_violations(&manifest, &names);
        assert!(
            violations.is_empty(),
            "crates/{member}/Cargo.toml takes core outside the workspace: {violations:#?}"
        );
    }
}

#[test]
fn core_tags_rejects_branch_rev_path_and_missing_tag() {
    for line in [
        r#"x = { git = "https://github.com/faberline/core", branch = "main" }"#,
        r#"x = { git = "https://github.com/faberline/core", rev = "abc" }"#,
        r#"x = { path = "../core", git = "https://github.com/faberline/core" }"#,
        r#"x = { git = "https://github.com/faberline/core" }"#,
    ] {
        assert!(core_tags(line).is_err(), "accepted: {line}");
    }
}

#[test]
fn core_tags_rejects_prerelease_tag() {
    let prerelease = r#"x = { git = "https://github.com/faberline/core", tag = "v0.5.0-rc.1" }"#;
    assert!(core_tags(prerelease).is_err(), "accepted a prerelease tag");
    let stable = r#"x = { git = "https://github.com/faberline/core", tag = "v0.5.0" }"#;
    assert_eq!(core_tags(stable), Ok(vec!["v0.5.0".to_string()]));
}

#[test]
fn one_tag_rejects_two_tags() {
    assert!(one_tag(&["v0.5.0".into(), "v0.4.14".into()]).is_err());
    assert!(one_tag(&[]).is_err());
    assert_eq!(
        one_tag(&["v0.5.0".into(), "v0.5.0".into()]),
        Ok("v0.5.0".to_string())
    );
}

#[test]
fn member_core_violations_flags_git_source_and_version() {
    let names = vec!["raft-runtime".to_string()];
    let git = "[dependencies]\nraft-runtime = { git = \"https://github.com/faberline/core\", tag = \"v0.5.0\" }\n";
    assert_eq!(member_core_violations(git, &names).len(), 1);
    let plain = "[dependencies]\nraft-runtime = \"1\"\n";
    assert_eq!(member_core_violations(plain, &names).len(), 1);
    let inherited =
        "[dependencies]\nraft-runtime = { workspace = true, optional = true }\nraft-runtime.workspace = true\n";
    assert!(member_core_violations(inherited, &names).is_empty());
    let package = "[package]\nname = \"raft-runtime\"\n";
    assert!(member_core_violations(package, &names).is_empty());
}

#[test]
fn core_pin_is_current_release() {
    let tags = core_tags(&read("Cargo.toml")).unwrap_or_else(|error| panic!("{error}"));
    let tag = one_tag(&tags).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(tag, CORE_PIN);
}

#[test]
fn lock_core_sources_match_pin() {
    let lock = read("Cargo.lock");
    let lines: Vec<&str> = lock
        .lines()
        .filter(|line| line.contains("github.com/faberline/core?tag="))
        .collect();
    assert!(
        lines.len() >= 19,
        "expected at least 19 core sources, found {}",
        lines.len()
    );
    let expected = format!("faberline/core?tag={CORE_PIN}#");
    for line in lines {
        assert!(
            line.contains(&expected),
            "core source is not {CORE_PIN}: {line}"
        );
    }
}
