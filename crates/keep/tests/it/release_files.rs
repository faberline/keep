//! The milestone release gate files (`.github/aw-release.json` and
//! `.github/aw-release-gates.json`) must parse, name the release workflow's
//! jobs and cover the three test kinds.

use serde_json::Value;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

fn read(path: &str) -> String {
    let full = format!("{ROOT}/{path}");
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {full}: {e}"))
}

fn json(path: &str) -> Value {
    serde_json::from_str(&read(path)).unwrap_or_else(|e| panic!("{path} is not JSON: {e}"))
}

#[test]
fn release_files_are_valid_and_match_the_workflow() {
    let contract = json(".github/aw-release.json");
    let gates = json(".github/aw-release-gates.json");

    // (a) both files parse and name the repository and the version.
    assert_eq!(contract["repo"], "faberline/keep");
    assert_eq!(contract["version"], 1);
    assert_eq!(gates["repo"], "faberline/keep");
    assert_eq!(gates["version"], 1);

    // (b) one workflow, the release workflow; every test kind has a job.
    let workflows = gates["workflows"]
        .as_array()
        .expect("workflows is an array");
    assert_eq!(workflows.len(), 1, "the policy must name one workflow");
    assert_eq!(workflows[0]["path"], ".github/workflows/release.yml");
    let jobs = workflows[0]["jobs"].as_object().expect("jobs is an object");
    let kinds: Vec<&str> = jobs.values().map(|v| v.as_str().unwrap_or("")).collect();
    for kind in ["unit", "functional_e2e", "performance"] {
        assert!(kinds.contains(&kind), "no job of kind {kind} in the policy");
    }

    // (c) every check of every acceptance goal is a job of the policy.
    let milestone = &contract["milestones"]["4"];
    let goals = milestone["acceptance"]
        .as_array()
        .expect("milestone 4 has an acceptance array");
    for goal in goals {
        for check in goal["checks"].as_array().expect("checks is an array") {
            let name = check.as_str().expect("a check is a string");
            assert!(
                jobs.contains_key(name),
                "check {name:?} is not a job of the gates policy"
            );
        }
    }

    // (d) milestone 4 is the 0.8.0 release, published as a GitHub release,
    // and each goal names at least one issue.
    assert_eq!(milestone["version"], "0.8.0");
    assert_eq!(milestone["publication"], "github-release");
    for goal in goals {
        let issues = goal["issues"].as_array().expect("issues is an array");
        assert!(!issues.is_empty(), "goal {} has no issues", goal["goal"]);
    }

    // (e) the release workflow has the jobs the policy names.
    let release = read(".github/workflows/release.yml");
    assert!(
        release.contains("name: \"perf: competitor gate\""),
        "release.yml has no job named `perf: competitor gate`"
    );
    assert!(
        release.contains("name: preflight"),
        "release.yml has no job named `preflight`"
    );
    assert!(
        release.contains("uses: ./.github/workflows/ci.yml"),
        "release.yml does not call ci.yml"
    );
}
