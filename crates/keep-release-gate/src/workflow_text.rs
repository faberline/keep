//! Text helpers for the release workflows (ported from verify-release-contract.py).

use crate::contract::{fail, Result};
use crate::util::is_lower_hex;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

static TRIGGER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^  ([a-z_]+):$").expect("valid regex"));
static INPUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^      ([a-z][a-z0-9_]*):$").expect("valid regex"));
static JOB: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^  ([a-z][a-z0-9-]*):$").expect("valid regex"));
static JOB_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^  [a-z][a-z0-9-]*:$").expect("valid regex"));
static USES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:-\s+)?uses:\s*(\S+)").expect("valid regex"));
static NEEDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*needs: \[([^\]]+)\]$").expect("valid regex"));

/// The number of leading white space characters of `line`.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// True for a line that starts a block run script (`run: |` or `run: |-`).
fn is_block_run(line: &str) -> bool {
    matches!(line.trim(), "run: |" | "run: |-")
}

/// The lines after `start` that are blank or indented more than the line at `start`.
fn block_body(lines: &[&str], start: usize) -> String {
    let ind = indent(lines[start]);
    lines[start + 1..]
        .iter()
        .take_while(|line| line.trim().is_empty() || indent(line) > ind)
        .copied()
        .collect::<Vec<&str>>()
        .join("\n")
}

/// The top-level block `key:` of a workflow, up to the next top-level key.
pub fn top_block(text: &str, key: &str) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|line| *line == format!("{key}:")) else {
        return fail(format!("top-level key '{key}' is absent"));
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| !line.is_empty() && !line.starts_with(' ') && !line.starts_with('#'))
        .map_or(lines.len(), |i| start + 1 + i);
    Ok(lines[start..end].join("\n"))
}

/// The trigger names of the `on` block.
pub fn workflow_triggers(text: &str) -> Result<Vec<String>> {
    Ok(TRIGGER
        .captures_iter(&top_block(text, "on")?)
        .map(|c| c[1].to_string())
        .collect())
}

/// The workflow input names of the `on` block.
pub fn workflow_inputs(text: &str) -> Result<BTreeSet<String>> {
    Ok(INPUT
        .captures_iter(&top_block(text, "on")?)
        .map(|c| c[1].to_string())
        .collect())
}

/// The job ids of the `jobs` block.
pub fn job_ids(text: &str) -> Result<BTreeSet<String>> {
    Ok(JOB
        .captures_iter(&top_block(text, "jobs")?)
        .map(|c| c[1].to_string())
        .collect())
}

/// The block of one job, up to the next job.
pub fn job_block(text: &str, job: &str) -> Result<String> {
    let jobs = top_block(text, "jobs")?;
    let lines: Vec<&str> = jobs.lines().collect();
    let Some(start) = lines.iter().position(|line| *line == format!("  {job}:")) else {
        return fail(format!("job {job} is absent"));
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| JOB_LINE.is_match(line))
        .map_or(lines.len(), |i| start + 1 + i);
    Ok(lines[start..end].join("\n"))
}

/// The block of one step, from its `- name:` line up to the next step at the same level.
pub fn step_block(job_text: &str, step_name: &str) -> Result<String> {
    let lines: Vec<&str> = job_text.lines().collect();
    let marker = format!("- name: {step_name}");
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == marker)
        .map(|(i, _)| i)
        .collect();
    if starts.len() != 1 {
        return fail(format!(
            "step '{step_name}' must appear exactly once, found {}",
            starts.len()
        ));
    }
    let start = starts[0];
    let ind = indent(lines[start]);
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            !line.trim().is_empty() && indent(line) <= ind && line.trim_start().starts_with("- ")
        })
        .map_or(lines.len(), |i| start + 1 + i);
    Ok(lines[start..end].join("\n"))
}

/// The body of the one block run script (`run: |`) of a step.
pub fn run_script(step_text: &str) -> Result<String> {
    let lines: Vec<&str> = step_text.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_block_run(line))
        .map(|(i, _)| i)
        .collect();
    if starts.len() != 1 {
        return fail("step must carry exactly one block run script");
    }
    Ok(block_body(&lines, starts[0]))
}

/// Every run script of a workflow, in order: block bodies and inline `run:` values.
pub fn all_run_scripts(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut scripts = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if is_block_run(line) {
            scripts.push(block_body(&lines, index));
        } else if let Some(rest) = line.trim().strip_prefix("run: ") {
            scripts.push(rest.to_string());
        }
    }
    scripts
}

/// The `uses:` values of a workflow.
pub fn action_values(text: &str) -> Vec<String> {
    USES.captures_iter(text).map(|c| c[1].to_string()).collect()
}

/// Refuse any action (other than a local `./` one) that is not pinned to a 40-hex commit.
pub fn assert_actions_pinned(label: &str, text: &str) -> Result<()> {
    for value in action_values(text) {
        if value.starts_with("./") {
            continue;
        }
        let pinned = value
            .rsplit_once('@')
            .is_some_and(|(_, sha)| is_lower_hex(sha, 40));
        if !pinned {
            return fail(format!(
                "{label}: action not pinned to a 40-hex commit: {value}"
            ));
        }
    }
    Ok(())
}

/// The job ids of a bracketed `needs: [...]` list.
pub fn parse_needs(job_text: &str) -> Result<Vec<String>> {
    let Some(caps) = NEEDS.captures(job_text) else {
        return fail("result job must declare a bracketed needs list");
    };
    Ok(caps[1]
        .split(',')
        .map(|item| item.trim().to_string())
        .collect())
}

/// The key and value lines of a job's `with:` block, with one pair of quotes removed from each value.
pub fn with_values(job_text: &str) -> Result<BTreeMap<String, String>> {
    let lines: Vec<&str> = job_text.lines().collect();
    let Some(at) = lines.iter().position(|line| *line == "    with:") else {
        return fail("release job must have a with: block");
    };
    let mut values = BTreeMap::new();
    for line in &lines[at + 1..] {
        if line.trim().is_empty() {
            continue;
        }
        if indent(line) <= 4 {
            break;
        }
        let trimmed = line.trim();
        let Some((key, raw)) = trimmed.split_once(':') else {
            return fail(format!("with: line is not key: value: '{trimmed}'"));
        };
        let mut value = raw.trim();
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = &value[1..value.len() - 1];
        }
        if values.contains_key(key) {
            return fail(format!("with: key {key} repeats"));
        }
        values.insert(key.to_string(), value.to_string());
    }
    Ok(values)
}

/// The lines of a job's `permissions:` block, trimmed.
pub fn permission_pairs(job_text: &str) -> Result<Vec<String>> {
    let lines: Vec<&str> = job_text.lines().collect();
    let Some(at) = lines.iter().position(|line| *line == "    permissions:") else {
        return fail("release job must declare its permissions");
    };
    let mut pairs = Vec::new();
    for line in &lines[at + 1..] {
        if line.trim().is_empty() {
            continue;
        }
        if indent(line) <= 4 {
            break;
        }
        pairs.push(line.trim().to_string());
    }
    Ok(pairs)
}
