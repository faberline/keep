//! The quickstart document runs against the image that CI builds (#238).
//!
//! `quickstart_lines_are_found` reads the commands of `docs/product/quickstart.md`
//! and fails when the document loses or changes the lines the image test runs.
//! `quickstart_runs_on_the_image` runs those same lines against the `keep:local`
//! image, which CI builds from the source `Dockerfile`. It needs docker, so it is
//! `#[ignore]`; the CI job `test-quickstart-image` runs it.

use std::path::Path;
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::Duration;

const CONTAINER: &str = "keep-quickstart-test";

fn doc() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/product/quickstart.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The non-empty lines inside ```` ```bash ```` fences, trimmed.
fn bash_lines(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_bash = false;
    for line in md.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_bash = t == "```bash";
            continue;
        }
        if in_bash && !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out
}

/// The one `docker run` line of the quickstart.
fn run_line(lines: &[String]) -> String {
    let runs: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("docker run "))
        .collect();
    assert_eq!(
        runs.len(),
        1,
        "quickstart needs exactly one `docker run` line: {runs:?}"
    );
    runs[0].clone()
}

/// The one `curl` line of the quickstart that contains `needle`.
fn curl_line(lines: &[String], needle: &str) -> String {
    let hits: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("curl ") && l.contains(needle))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "quickstart needs exactly one curl line containing `{needle}`: {hits:?}"
    );
    hits[0].clone()
}

/// The body that the quickstart says the GET returns.
fn expected_get_body(md: &str) -> String {
    let marker = "The response is `";
    let start = md
        .find(marker)
        .expect("quickstart says what the GET returns")
        + marker.len();
    let end = md[start..]
        .find('`')
        .expect("the GET body is closed by a backtick")
        + start;
    md[start..end].to_string()
}

/// Runs one quickstart curl line through `sh`, with `-sf` so a failed status fails.
fn sh_curl(line: &str) -> Output {
    Command::new("sh")
        .arg("-c")
        .arg(line.replacen("curl ", "curl -sf ", 1))
        .output()
        .expect("run sh")
}

#[test]
fn quickstart_lines_are_found() {
    let md = doc();
    let lines = bash_lines(&md);

    let run = run_line(&lines);
    for part in [
        "keep:local",
        "-p 7117:7117",
        "KEEP_HOST=0.0.0.0",
        "--disable-persistence",
    ] {
        assert!(run.contains(part), "docker run line lacks `{part}`: {run}");
    }

    let put = curl_line(&lines, "-X PUT http://127.0.0.1:7117/kv/hello");
    assert!(
        put.contains(r#"{"value":"world"}"#),
        "PUT line changed: {put}"
    );
    assert_eq!(expected_get_body(&md), r#"{"key":"hello","value":"world"}"#);
    curl_line(&lines, "curl http://127.0.0.1:7117/kv/hello");
    curl_line(&lines, "http://127.0.0.1:7117/healthz");
    curl_line(&lines, "http://127.0.0.1:7117/readyz");
}

/// Removes the test container when the test ends, also when it panics.
struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", CONTAINER])
            .output();
    }
}

#[test]
#[ignore = "needs docker and the keep:local image; CI job test-quickstart-image runs it"]
fn quickstart_runs_on_the_image() {
    let md = doc();
    let lines = bash_lines(&md);
    let _cleanup = Cleanup;

    // The document's own `docker run` line, with only the container name added.
    let run = run_line(&lines);
    let mut tokens = run.split_whitespace();
    assert_eq!(tokens.next(), Some("docker"));
    assert_eq!(tokens.next(), Some("run"));
    let args: Vec<&str> = tokens.collect();
    let started = Command::new("docker")
        .args(["run", "-d", "--name", CONTAINER])
        .args(&args)
        .output()
        .expect("run docker");
    assert!(
        started.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let readyz = curl_line(&lines, "http://127.0.0.1:7117/readyz");
    let mut ready = false;
    for _ in 0..60 {
        if sh_curl(&readyz).status.success() {
            ready = true;
            break;
        }
        sleep(Duration::from_millis(500));
    }
    if !ready {
        let logs = Command::new("docker")
            .args(["logs", CONTAINER])
            .output()
            .expect("docker logs");
        panic!(
            "keep did not become ready: {}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
    }

    // One write, then one read, both as the document gives them.
    let put = sh_curl(&curl_line(&lines, "-X PUT http://127.0.0.1:7117/kv/hello"));
    assert!(
        put.status.success(),
        "PUT failed: {}",
        String::from_utf8_lossy(&put.stderr)
    );
    let get = sh_curl(&curl_line(&lines, "curl http://127.0.0.1:7117/kv/hello"));
    assert!(
        get.status.success(),
        "GET failed: {}",
        String::from_utf8_lossy(&get.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&get.stdout).trim(),
        expected_get_body(&md)
    );

    let health = sh_curl(&curl_line(&lines, "http://127.0.0.1:7117/healthz"));
    assert!(health.status.success(), "healthz failed");
    assert!(String::from_utf8_lossy(&health.stdout).contains("ok"));
}
