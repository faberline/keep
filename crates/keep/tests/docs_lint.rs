//! Doc-lint for the product docs (`docs/product/quickstart.md`,
//! `docs/product/runbook.md`): every `keep` command in a bash fence must parse
//! against the real CLI help, and every `--flag` it names must appear in that
//! help. Offline: only the compiled `keep` binary runs. `curl`, `docker` and
//! `kubectl` lines are not checked here; the pages say so.

use std::path::Path;
use std::process::Command;

fn doc(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|error| panic!("read {}: {error}", full.display()))
}

/// One `keep` command found in a bash fence: its subcommand words and flags.
#[derive(Debug, PartialEq)]
struct KeepCommand {
    subcommand: Vec<String>,
    flags: Vec<String>,
}

/// A subcommand word starts with a lowercase letter and holds only `[a-z-]`.
fn is_subcommand_word(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_lowercase())
        && word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Parse one joined line. Only a line whose first word is `keep` counts.
fn parse_keep_line(line: &str) -> Option<KeepCommand> {
    let mut words = line.split_whitespace().peekable();
    if words.next()? != "keep" {
        return None;
    }
    let mut subcommand = Vec::new();
    let mut flags = Vec::new();
    let mut seen_flag = false;
    while let Some(word) = words.next() {
        if word.starts_with('-') {
            seen_flag = true;
            let name = word.split('=').next().unwrap_or(word);
            flags.push(name.to_string());
            // `--flag value`: the next word that does not start with `-` is the value.
            if !word.contains('=') {
                if let Some(next) = words.peek() {
                    if !next.starts_with('-') {
                        words.next();
                    }
                }
            }
        } else if !seen_flag && is_subcommand_word(word) {
            subcommand.push(word.to_string());
        }
    }
    Some(KeepCommand { subcommand, flags })
}

/// Every `keep` command in the bash fences of `md`. A line that ends in `\`
/// joins the next line.
fn keep_commands(md: &str) -> Vec<KeepCommand> {
    let mut commands = Vec::new();
    let mut in_bash = false;
    let mut joined = String::new();
    for raw in md.lines() {
        let line = raw.trim();
        if !in_bash {
            in_bash = line == "```bash";
            continue;
        }
        if line == "```" {
            in_bash = false;
            joined.clear();
            continue;
        }
        if let Some(head) = line.strip_suffix('\\') {
            joined.push_str(head.trim_end());
            joined.push(' ');
            continue;
        }
        joined.push_str(line);
        if let Some(command) = parse_keep_line(&std::mem::take(&mut joined)) {
            commands.push(command);
        }
    }
    commands
}

/// The `--help` text of `keep <subcommand...>`, or an error when it fails.
fn help_for(subcommand: &[String]) -> Result<String, String> {
    let out = Command::new(env!("CARGO_BIN_EXE_keep"))
        .args(subcommand)
        .arg("--help")
        .output()
        .map_err(|error| format!("run keep: {error}"))?;
    if !out.status.success() {
        return Err(format!(
            "`keep {} --help` failed: {}",
            subcommand.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Ok when every `keep` command in `md` is accepted and each of its flags is in the help.
fn check(md: &str) -> Result<(), String> {
    for command in keep_commands(md) {
        let help = help_for(&command.subcommand)?;
        for flag in &command.flags {
            if !help.contains(flag.as_str()) {
                return Err(format!(
                    "`keep {} {flag}` is not in its --help",
                    command.subcommand.join(" ")
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn quickstart_keep_commands_match_cli_help() {
    let md = doc("docs/product/quickstart.md");
    assert!(
        !keep_commands(&md).is_empty(),
        "quickstart has no keep command"
    );
    check(&md).unwrap();
}

#[test]
fn runbook_keep_commands_match_cli_help() {
    let md = doc("docs/product/runbook.md");
    assert!(
        !keep_commands(&md).is_empty(),
        "runbook has no keep command"
    );
    check(&md).unwrap();
}

#[test]
fn docs_state_what_is_not_tested() {
    for path in ["docs/product/quickstart.md", "docs/product/runbook.md"] {
        let md = doc(path).to_lowercase();
        assert!(
            md.contains("not tested"),
            "{path} does not say what is not tested"
        );
    }
}

#[test]
fn readme_links_both_pages() {
    let readme = doc("README.md");
    assert!(readme.contains("docs/product/quickstart.md"));
    assert!(readme.contains("docs/product/runbook.md"));
}

#[test]
fn keep_commands_rejects_unknown_flag() {
    assert!(check("```bash\nkeep --no-such-flag\n```").is_err());
    assert!(check("```bash\nkeep backup --no-such-flag\n```").is_err());
    // Positive control: a real flag passes.
    assert!(check("```bash\nkeep --port 7117\n```").is_ok());
}

#[test]
fn keep_commands_skips_other_tools() {
    let md = "```bash\ndocker run keep:local\ncurl http://x\n# keep --bad\n```";
    assert!(keep_commands(md).is_empty());
}

#[test]
fn performance_doc_matches_baseline() {
    let baseline_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/perf-baseline.json");
    let raw = std::fs::read_to_string(&baseline_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", baseline_path.display()));
    let baseline: serde_json::Value =
        serde_json::from_str(&raw).expect("perf-baseline.json is valid JSON");
    let md = doc("docs/product/performance.md");

    let policy = baseline["policy"].as_object().expect("policy is an object");
    for (name, value) in policy {
        let line = format!("- `{name}`: {value}");
        assert!(
            md.lines().any(|l| l.trim() == line),
            "performance.md lacks the policy line `{line}`"
        );
    }

    let hosts = baseline["hosts"].as_object().expect("hosts is an object");
    for host in hosts.keys() {
        assert!(
            md.contains(host.as_str()),
            "performance.md does not name the host class `{host}`"
        );
    }

    for (host, data) in hosts {
        let cells = data["cells"].as_object().expect("cells is an object");
        for (key, cell) in cells {
            let name = format!("`{key}`");
            let values = format!("| {} | {} |", cell["baseline"], cell["min_ratio"]);
            assert!(
                md.lines().any(|l| l.contains(&name) && l.contains(&values)),
                "performance.md lacks the cell {key} of {host} with values `{values}`"
            );
        }
    }

    assert!(
        md.contains("43578822818"),
        "performance.md does not name the recording run 43578822818"
    );
    assert!(
        md.contains("not recorded"),
        "performance.md does not say which numbers are not recorded"
    );
}
