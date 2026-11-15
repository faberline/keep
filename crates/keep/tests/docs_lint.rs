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
    for path in [
        "docs/product/quickstart.md",
        "docs/product/runbook.md",
        "docs/product/alerts.md",
    ] {
        let md = doc(path).to_lowercase();
        assert!(
            md.contains("not tested"),
            "{path} does not say what is not tested"
        );
    }
}

/// GitHub anchor of a heading: lowercase, spaces become `-`, other characters
/// outside ASCII letters, digits and `-` are dropped.
fn heading_anchor(heading: &str) -> String {
    let mut out = String::new();
    for c in heading.trim().to_lowercase().chars() {
        if c == ' ' {
            out.push('-');
        } else if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            out.push(c);
        }
    }
    out
}

/// The anchor of each `## ` heading in `md`, skipping lines inside a fence.
fn headings(md: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut out = Vec::new();
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(text) = line.strip_prefix("## ") {
            out.push(heading_anchor(text));
        }
    }
    out
}

/// The text under `## <heading>` up to the next `## ` heading, if the heading exists.
fn section(md: &str, heading: &str) -> Option<String> {
    let mut in_section = false;
    let mut out = String::new();
    for line in md.lines() {
        if let Some(text) = line.strip_prefix("## ") {
            if in_section {
                break;
            }
            in_section = text.trim() == heading;
            continue;
        }
        if in_section {
            out.push_str(line);
            out.push('\n');
        }
    }
    in_section.then_some(out)
}

/// The anchor of every `](runbook.md#anchor)` link in `alerts`.
fn runbook_link_anchors(alerts: &str) -> Vec<String> {
    const PREFIX: &str = "](runbook.md#";
    let mut anchors = Vec::new();
    let mut rest = alerts;
    while let Some(at) = rest.find(PREFIX) {
        let after = &rest[at + PREFIX.len()..];
        let end = after.find(')').unwrap_or(after.len());
        anchors.push(after[..end].to_string());
        rest = &after[end..];
    }
    anchors
}

/// The anchors that `alerts` links to in `runbook` but that `runbook` has no heading for.
fn broken_runbook_links(alerts: &str, runbook: &str) -> Vec<String> {
    let known = headings(runbook);
    runbook_link_anchors(alerts)
        .into_iter()
        .filter(|anchor| !known.contains(anchor))
        .collect()
}

#[test]
fn runbook_has_upgrade_section() {
    let runbook = doc("docs/product/runbook.md");
    let upgrade = section(&runbook, "Upgrade").expect("runbook has no `## Upgrade` section");
    for needle in ["/readyz", "raftz", "rollout undo", "not tested"] {
        assert!(
            upgrade.contains(needle),
            "`## Upgrade` does not mention `{needle}`"
        );
    }
}

#[test]
fn alerts_doc_links_resolve() {
    let alerts = doc("docs/product/alerts.md");
    let runbook = doc("docs/product/runbook.md");
    assert!(
        runbook_link_anchors(&alerts).len() >= 5,
        "alerts.md has fewer than 5 runbook links"
    );
    assert_eq!(
        broken_runbook_links(&alerts, &runbook),
        Vec::<String>::new()
    );
    assert_eq!(
        headings(&alerts).len(),
        5,
        "alerts.md must have exactly five `##` headings"
    );
    for title in [
        "Node down",
        "Disk full",
        "Replica behind",
        "Readiness failing",
        "Token rejects",
    ] {
        let body = section(&alerts, title)
            .unwrap_or_else(|| panic!("alerts.md has no `## {title}` heading"));
        for label in ["Symptom:", "Check:", "Action:"] {
            assert!(
                body.lines().any(|line| line.starts_with(label)),
                "`## {title}` has no `{label}` line"
            );
        }
    }
}

#[test]
fn alerts_link_check_rejects_a_missing_anchor() {
    let runbook = "# Runbook\n\n## Node down\n";
    assert_eq!(
        broken_runbook_links("[x](runbook.md#nope)", runbook),
        vec!["nope".to_string()]
    );
    assert!(broken_runbook_links("[x](runbook.md#node-down)", runbook).is_empty());
}

#[test]
fn readme_links_both_pages() {
    let readme = doc("README.md");
    assert!(readme.contains("docs/product/quickstart.md"));
    assert!(readme.contains("docs/product/runbook.md"));
}

#[test]
fn threat_model_has_required_sections() {
    let md = doc("docs/product/threat-model.md");
    let required = [
        "## Assets",
        "## Trust boundaries",
        "## Threats",
        "## Mitigations",
        "## Residual risk",
        "## Security review",
    ];
    // Exactly these `##` headings, in this order, and no others.
    let headings: Vec<&str> = md.lines().filter(|line| line.starts_with("## ")).collect();
    assert_eq!(
        headings, required,
        "threat-model.md must have exactly these ## sections, in order"
    );
    let (_, review) = md
        .split_once("\n## Security review\n")
        .expect("threat-model.md has no Security review section");
    assert!(
        review.contains("- [ ] Independent security review"),
        "Security review section lacks the unchecked review line"
    );
    assert!(
        !md.contains("- [x]"),
        "threat-model.md must not mark a review as done"
    );
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

/// The long flags (`--name`) named in `help`, sorted and without duplicates.
/// `--help` and `--version` are left out: every command has them.
fn long_flags(help: &str) -> Vec<String> {
    let mut flags: Vec<String> = help
        .split_whitespace()
        .map(|word| word.trim_end_matches([',', '.']))
        .filter(|word| {
            word.starts_with("--")
                && word.len() > 2
                && word[2..]
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
        .filter(|word| *word != "--help" && *word != "--version")
        .map(str::to_string)
        .collect();
    flags.sort();
    flags.dedup();
    flags
}

/// The long flags of `help` whose name does not appear in `doc`.
fn flags_missing_from(help: &str, doc: &str) -> Vec<String> {
    long_flags(help)
        .into_iter()
        .filter(|flag| !doc.contains(flag.as_str()))
        .collect()
}

#[test]
fn configuration_flags_helper_reports_a_missing_flag() {
    let help = "  --host <H>\n  --port <P>\n  -h, --help";
    let missing = flags_missing_from(help, "`--host` is the address");
    assert_eq!(missing, vec!["--port".to_string()]);
}

#[test]
fn configuration_doc_lists_every_flag() {
    let help = help_for(&[]).unwrap();
    assert!(
        long_flags(&help).len() >= 20,
        "keep --help lists too few flags: {:?}",
        long_flags(&help)
    );
    let missing = flags_missing_from(&help, &doc("docs/product/configuration.md"));
    assert!(
        missing.is_empty(),
        "configuration.md misses flags: {missing:?}"
    );
}

#[test]
fn limits_doc_has_sections() {
    let md = doc("docs/product/limits.md");
    for section in ["## Sizes", "## Rates", "## Known limits"] {
        assert!(
            md.contains(section),
            "limits.md lacks the section `{section}`"
        );
    }
}

/// The text under `## <heading>` up to the next `## ` heading, if the heading exists.
fn section(md: &str, heading: &str) -> Option<String> {
    let mut in_section = false;
    let mut out = String::new();
    for line in md.lines() {
        if let Some(text) = line.strip_prefix("## ") {
            if in_section {
                break;
            }
            in_section = text.trim() == heading;
            continue;
        }
        if in_section {
            out.push_str(line);
            out.push('\n');
        }
    }
    in_section.then_some(out)
}

/// One gate row of the `## Gates` table: (gate, where keep does it, status).
fn gate_rows(md: &str) -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for line in section(md, "Gates").unwrap_or_default().lines() {
        let line = line.trim();
        if !line.starts_with('|') || line.contains("---") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() != 3 || cells[0] == "Gate" {
            continue;
        }
        rows.push((
            cells[0].to_string(),
            cells[1].to_string(),
            cells[2].to_string(),
        ));
    }
    rows
}

/// The text inside each pair of backticks in `cell`.
fn backtick_spans(cell: &str) -> Vec<String> {
    cell.split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, span)| span.to_string())
        .collect()
}

/// The status of each row that is neither `present` nor `missing`.
fn bad_status(rows: &[(String, String, String)]) -> Vec<String> {
    rows.iter()
        .map(|(_, _, status)| status.clone())
        .filter(|status| status != "present" && status != "missing")
        .collect()
}

/// True when the needle named in a `present` Where cell does not appear in its file.
fn missing_needle(root: &Path, where_cell: &str) -> bool {
    let spans = backtick_spans(where_cell);
    let text = std::fs::read_to_string(root.join(&spans[0])).unwrap_or_default();
    !text.contains(spans[1].as_str())
}

#[test]
fn release_gaps_doc_has_every_gate() {
    let md = doc("docs/product/release-gaps.md");
    assert!(
        section(&md, "Gates").is_some(),
        "release-gaps.md lacks `## Gates`"
    );
    let missing_section =
        section(&md, "Missing gates").expect("release-gaps.md lacks `## Missing gates`");
    let rows = gate_rows(&md);
    assert_eq!(rows.len(), 24, "release-gaps.md must have 24 gate rows");
    assert_eq!(bad_status(&rows), Vec::<String>::new());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut missing = 0;
    for (gate, where_cell, status) in &rows {
        if status == "present" {
            let spans = backtick_spans(where_cell);
            assert_eq!(
                spans.len(),
                2,
                "`{gate}`: a present row needs a path and a needle"
            );
            let text = std::fs::read_to_string(root.join(&spans[0]))
                .unwrap_or_else(|error| panic!("`{gate}`: read {}: {error}", spans[0]));
            assert!(
                text.contains(spans[1].as_str()),
                "`{gate}`: {} does not contain `{}`",
                spans[0],
                spans[1]
            );
        } else {
            missing += 1;
            assert!(
                missing_section.contains(gate.as_str()),
                "`{gate}` is missing but not listed under `## Missing gates`"
            );
        }
    }
    assert_eq!(missing, 4, "release-gaps.md must list 4 missing gates");
}

#[test]
fn release_gaps_rejects_bad_status() {
    let md = "## Gates\n\n| Gate | Where keep does it | Status |\n|---|---|---|\n| g | `a` `b` | maybe |\n";
    let rows = gate_rows(md);
    assert_eq!(rows.len(), 1);
    assert_eq!(bad_status(&rows), vec!["maybe".to_string()]);
    let good = [
        (
            "g".to_string(),
            "`a` `b`".to_string(),
            "present".to_string(),
        ),
        ("g".to_string(), "`a`".to_string(), "missing".to_string()),
    ];
    assert!(bad_status(&good).is_empty());
}

#[test]
fn release_gaps_present_row_needs_real_needle() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(missing_needle(
        &root,
        "`.github/workflows/keep-release.yml` `no-such-step-name`"
    ));
    assert!(!missing_needle(
        &root,
        "`.github/workflows/keep-release.yml` `service-release.yml@`"
    ));
}
