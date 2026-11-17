//! The build-release skill checks, ported from `scripts/release/verify-release-contract.py`.

use crate::refusal::{refuse, Refusal, Result};
use crate::util::sha256_hex;
use std::path::Path;

const LABEL: &str = "build-release skill";
const AGENTS_SKILL: &str = ".agents/skills/build-release/SKILL.md";
const CLAUDE_SKILL: &str = ".claude/skills/build-release/SKILL.md";
const CANDIDATE: &str = "3. Dispatch `<app>-release-candidate`";
const VERIFIER_STEP: &str =
    "4. Independently run the candidate verifier in full mode. Stop on any mismatch.\n";
const TAG_STEP: &str =
    "7. The controller creates one annotated `<app>@<version>` tag at the exact\n";
const RECEIPT: &str = "Wait for the final candidate manifest.";

/// The bytes of the build-release skill are pinned: a change that keeps both copies equal
/// still has to be re-read and re-pinned here.
pub const RELEASE_SKILL_SHA256: &str =
    "16bec3f6a894890c0a8312ac38207275d71659598a27b456d504468880fbaeab";

pub const SKILL_ORDER: [&str; 7] = [
    "2. Run `git-land`",
    "3. Dispatch `<app>-release-candidate`",
    "Wait for the final candidate manifest.",
    "4. Independently run the candidate verifier in full mode.",
    "7. The controller creates one annotated `<app>@<version>` tag",
    "8. Dispatch `<app>-release`",
    "9. Run the public verifier.",
];

pub const RETIRED_RELEASE_ROUTES: [&str; 2] = [
    ".claude/skills/lumen-build-release/scripts/release.sh",
    "scripts/project-build-monitor-release.sh",
];

/// The Python `repr` of a string, for the messages that quote an anchor.
pub fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Replace `old` by `new` when `old` occurs exactly once in `text`.
pub fn replace_once(text: &str, old: &str, new: &str, label: &str) -> Result<String> {
    let count = text.matches(old).count();
    if count != 1 {
        return refuse(format!(
            "{label}: mutation anchor must appear exactly once: {} ({count})",
            py_repr(old)
        ));
    }
    Ok(text.replace(old, new))
}

/// Non-comment lines with backslash continuations joined, as a shell reads them.
pub fn logical_lines(text: &str) -> Vec<String> {
    let mut logical = Vec::new();
    let mut current = String::new();
    for raw in text.split('\n') {
        let trimmed = raw.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(stripped) = trimmed.strip_suffix('\\') {
            current.push_str(stripped.trim_end());
            current.push(' ');
            continue;
        }
        current.push_str(trimmed);
        if !current.trim().is_empty() {
            logical.push(current.trim().to_string());
        }
        current.clear();
    }
    if !current.trim().is_empty() {
        logical.push(current.trim().to_string());
    }
    logical
}

/// Refuse a `git` command that runs `tag` or `push`, however it is spelled.
pub fn check_no_raw_git_tag_push(text: &str) -> Result<()> {
    for line in logical_lines(text) {
        let tokens: Vec<&str> = line
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|token| !token.is_empty())
            .collect();
        if let Some(git) = tokens.iter().position(|token| *token == "git") {
            if tokens[git + 1..]
                .iter()
                .any(|token| *token == "tag" || *token == "push")
            {
                return refuse(format!("{LABEL}: raw Git tag/push command remains: {line}"));
            }
        }
    }
    Ok(())
}

/// The steps of the skill appear in order, no retired route is left, and no raw tag or push.
pub fn check_skill_order(text: &str) -> Result<()> {
    let mut previous = 0usize;
    for marker in SKILL_ORDER {
        let Some(position) = text.find(marker) else {
            return refuse(format!("{LABEL} is missing '{marker}'"));
        };
        if position <= previous {
            return refuse(format!("{LABEL} order is invalid at '{marker}'"));
        }
        previous = position;
    }
    for retired in RETIRED_RELEASE_ROUTES {
        if text.contains(retired) {
            return refuse(format!("{LABEL}: retired release route remains: {retired}"));
        }
    }
    check_no_raw_git_tag_push(text)
}

/// Both entrypoints are equal, match the pinned digest, and keep the order.
pub fn check_skill_pair_with(agents: &str, claude: &str, expected_sha256: &str) -> Result<()> {
    if agents != claude {
        return refuse(format!(
            "{LABEL}: the .agents and .claude entrypoints differ"
        ));
    }
    let digest = sha256_hex(agents.as_bytes());
    if digest != expected_sha256 {
        return refuse(format!(
            "{LABEL} digest changed: expected {expected_sha256}, got {digest}"
        ));
    }
    check_skill_order(agents)
}

pub fn check_skill_pair(agents: &str, claude: &str) -> Result<()> {
    check_skill_pair_with(agents, claude, RELEASE_SKILL_SHA256)
}

/// Read one skill file under `root`. A missing or non-regular file is refused by its path.
fn read_skill(root: &Path, rel: &str) -> Result<String> {
    let path = root.join(rel);
    let is_regular = std::fs::symlink_metadata(&path)
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false);
    if !is_regular {
        return refuse(format!("missing file: {rel}"));
    }
    let bytes = std::fs::read(&path).map_err(|_| Refusal(format!("missing file: {rel}")))?;
    String::from_utf8(bytes).map_err(|_| Refusal(format!("{rel} is not UTF-8")))
}

pub fn check_release_skill_with(root: &Path, expected_sha256: &str) -> Result<()> {
    let agents = read_skill(root, AGENTS_SKILL)?;
    let claude = read_skill(root, CLAUDE_SKILL)?;
    check_skill_pair_with(&agents, &claude, expected_sha256)
}

pub fn check_release_skill(root: &Path) -> Result<()> {
    check_release_skill_with(root, RELEASE_SKILL_SHA256)
}

/// The text with one mutation applied at `before_candidate`: `line` is put before the candidate step.
fn before_candidate(text: &str, line: &str) -> Result<String> {
    replace_once(text, CANDIDATE, &format!("{line}\n{CANDIDATE}"), LABEL)
}

type Control<'a> = (String, Box<dyn Fn() -> Result<()> + 'a>);

fn control<'a>(name: String, check: impl Fn() -> Result<()> + 'a) -> Control<'a> {
    (name, Box::new(check))
}

/// Nine controls: each builds a broken copy of the skill and must be refused by its check.
pub fn skill_controls(text: &str) -> Vec<Control<'_>> {
    vec![
        control(
            format!("{LABEL}: same-byte shell-escaped tag and push in both entrypoints"),
            move || {
                let escaped = before_candidate(
                    text,
                    "g\\it tag -m release tape@<version>; g\\it push --tags",
                )?;
                check_skill_pair(&escaped, &escaped)
            },
        ),
        control(format!("{LABEL}: entrypoints drift"), move || {
            let changed = replace_once(
                text,
                "Publish one verified release",
                "Publish a different verified release",
                LABEL,
            )?;
            check_skill_pair(text, &changed)
        }),
        control(
            format!("{LABEL}: tag before the candidate verifier"),
            move || {
                let moved = replace_once(text, VERIFIER_STEP, "", LABEL)?;
                let moved = replace_once(
                    &moved,
                    TAG_STEP,
                    &format!("{TAG_STEP}{VERIFIER_STEP}"),
                    LABEL,
                )?;
                check_skill_order(&moved)
            },
        ),
        control(
            format!("{LABEL}: candidate manifest awaited after the tag"),
            move || {
                let moved = replace_once(text, RECEIPT, "", LABEL)?;
                let moved =
                    replace_once(&moved, TAG_STEP, &format!("{TAG_STEP}{RECEIPT}\n"), LABEL)?;
                check_skill_order(&moved)
            },
        ),
        control(
            format!("{LABEL}: raw tag and push before the candidate"),
            move || {
                check_skill_order(&before_candidate(
                    text,
                    "git tag -m release tape@<version>; git push --tags",
                )?)
            },
        ),
        control(
            format!("{LABEL}: raw tag and push behind git -C"),
            move || {
                check_skill_order(&before_candidate(
                    text,
                    "git -C . tag -m release tape@<version>; git -C . push --tags",
                )?)
            },
        ),
        control(
            format!("{LABEL}: raw tag split across continuations"),
            move || {
                check_skill_order(&before_candidate(
                    text,
                    "git \\\n-C . \\\ntag -m release tape@<version>",
                )?)
            },
        ),
        control(
            format!("{LABEL}: raw push split across continuations"),
            move || {
                check_skill_order(&before_candidate(
                    text,
                    "git \\\n--git-dir=.git \\\npush --tags",
                )?)
            },
        ),
        control(
            format!("{LABEL}: retired release route restored"),
            move || check_skill_order(&format!("{text}\n{}\n", RETIRED_RELEASE_ROUTES[0])),
        ),
    ]
}
