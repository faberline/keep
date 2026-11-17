//! Tests for the build-release skill checks (`skill_contract`).

use keep_release_gate::refusal::Refusal;
use keep_release_gate::skill_contract::{
    check_no_raw_git_tag_push, check_release_skill, check_release_skill_with, check_skill_order,
    check_skill_pair, check_skill_pair_with, logical_lines, replace_once, skill_controls,
};
use keep_release_gate::util::sha256_hex;
use std::fs;

const SKILL: &str = "# build-release\n1. Check the version.\n2. Run `git-land`.\n3. Dispatch `<app>-release-candidate`.\nWait for the final candidate manifest.\n4. Independently run the candidate verifier in full mode. Stop on any mismatch.\n5. Check the digest.\n6. Ask for approval.\n7. The controller creates one annotated `<app>@<version>` tag at the exact\n   candidate commit.\n8. Dispatch `<app>-release`.\n9. Run the public verifier.\nPublish one verified release.\n";

const PINNED: &str = "16bec3f6a894890c0a8312ac38207275d71659598a27b456d504468880fbaeab";

fn refusal_text<T: std::fmt::Debug>(result: Result<T, Refusal>) -> String {
    result.expect_err("expected a refusal").0
}

#[test]
fn fixed_skill_passes() {
    let digest = sha256_hex(SKILL.as_bytes());
    assert_eq!(check_skill_pair_with(SKILL, SKILL, &digest), Ok(()));
}

#[test]
fn digest_change_is_refused() {
    let other = sha256_hex(b"other");
    assert_eq!(
        refusal_text(check_skill_pair_with(SKILL, SKILL, &other)),
        format!(
            "build-release skill digest changed: expected {other}, got {}",
            sha256_hex(SKILL.as_bytes())
        )
    );
    assert!(
        refusal_text(check_skill_pair(SKILL, SKILL)).starts_with(&format!(
            "build-release skill digest changed: expected {PINNED}, got "
        ))
    );
    assert_eq!(
        refusal_text(check_skill_pair_with(
            SKILL,
            &SKILL.replace("Check", "Chock"),
            &sha256_hex(SKILL.as_bytes())
        )),
        "build-release skill: the .agents and .claude entrypoints differ"
    );
}

#[test]
fn release_skill_reads_two_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".agents/skills/build-release")).unwrap();
    fs::create_dir_all(root.join(".claude/skills/build-release")).unwrap();
    let agents = root.join(".agents/skills/build-release/SKILL.md");
    let claude = root.join(".claude/skills/build-release/SKILL.md");
    fs::write(&agents, SKILL).unwrap();
    fs::write(&claude, SKILL).unwrap();

    let digest = sha256_hex(SKILL.as_bytes());
    assert_eq!(check_release_skill_with(root, &digest), Ok(()));
    assert!(
        refusal_text(check_release_skill(root)).starts_with(&format!(
            "build-release skill digest changed: expected {PINNED}, got "
        ))
    );

    fs::remove_file(&claude).unwrap();
    assert_eq!(
        refusal_text(check_release_skill_with(root, &digest)),
        "missing file: .claude/skills/build-release/SKILL.md"
    );

    fs::write(&claude, SKILL).unwrap();
    fs::remove_file(&agents).unwrap();
    assert_eq!(
        refusal_text(check_release_skill_with(root, &digest)),
        "missing file: .agents/skills/build-release/SKILL.md"
    );

    fs::write(&agents, [0xff_u8, 0xfe]).unwrap();
    assert_eq!(
        refusal_text(check_release_skill_with(root, &digest)),
        ".agents/skills/build-release/SKILL.md is not UTF-8"
    );
}

#[test]
fn logical_lines_cases() {
    assert_eq!(
        logical_lines("# comment\nkeep one\n\n git \\\n-C . \\\ntag -m x\nlast \\"),
        vec!["keep one", "git -C . tag -m x", "last"]
    );
}

#[test]
fn raw_tag_push_cases() {
    assert_eq!(
        refusal_text(check_no_raw_git_tag_push("git status\ngit -C . tag v1")),
        "build-release skill: raw Git tag/push command remains: git -C . tag v1"
    );
    assert_eq!(
        check_no_raw_git_tag_push("push the tag first\ngit status"),
        Ok(())
    );
}

#[test]
fn order_cases() {
    assert_eq!(check_skill_order(SKILL), Ok(()));
    assert_eq!(
        refusal_text(check_skill_order(
            &SKILL.replace("9. Run the public verifier.", "")
        )),
        "build-release skill is missing '9. Run the public verifier.'"
    );
}

#[test]
fn replace_once_cases() {
    assert_eq!(
        refusal_text(replace_once("aXbXc", "X", "Y", "L")),
        "L: mutation anchor must appear exactly once: 'X' (2)"
    );
    assert_eq!(
        refusal_text(replace_once("abc", "z", "Y", "L")),
        "L: mutation anchor must appear exactly once: 'z' (0)"
    );
    assert_eq!(replace_once("a\nb", "a\nb", "c", "L"), Ok("c".to_string()));
    // Python repr of a string holding `'` and no `"` uses double quotes.
    assert_eq!(
        refusal_text(replace_once("a'b\nc", "x\ny'", "Y", "L")),
        "L: mutation anchor must appear exactly once: \"x\\ny'\" (0)"
    );
    // Python repr of a string holding both quote kinds uses single quotes and escapes `'`.
    assert_eq!(
        refusal_text(replace_once("", "a'b\"c", "Y", "L")),
        "L: mutation anchor must appear exactly once: 'a\\'b\"c' (0)"
    );
    // Backslash, tab and carriage return are escaped as Python does.
    assert_eq!(
        refusal_text(replace_once("", "a\\b\tc\rd", "Y", "L")),
        "L: mutation anchor must appear exactly once: 'a\\\\b\\tc\\rd' (0)"
    );
}

#[test]
fn controls_each_fail_with_the_python_message() {
    let controls = skill_controls(SKILL);
    assert_eq!(controls.len(), 9);
    let messages: Vec<String> = controls
        .iter()
        .map(|(_, check)| refusal_text(check()))
        .collect();
    let digest_prefix = format!("build-release skill digest changed: expected {PINNED}, got ");
    assert!(messages[0].starts_with(&digest_prefix));
    assert_eq!(
        messages[1],
        "build-release skill: the .agents and .claude entrypoints differ"
    );
    assert_eq!(
        messages[2],
        "build-release skill order is invalid at '7. The controller creates one annotated `<app>@<version>` tag'"
    );
    assert_eq!(
        messages[3],
        "build-release skill order is invalid at '4. Independently run the candidate verifier in full mode.'"
    );
    assert_eq!(
        messages[4],
        "build-release skill: raw Git tag/push command remains: git tag -m release tape@<version>; git push --tags"
    );
    assert_eq!(
        messages[5],
        "build-release skill: raw Git tag/push command remains: git -C . tag -m release tape@<version>; git -C . push --tags"
    );
    assert_eq!(
        messages[6],
        "build-release skill: raw Git tag/push command remains: git -C . tag -m release tape@<version>"
    );
    assert_eq!(
        messages[7],
        "build-release skill: raw Git tag/push command remains: git --git-dir=.git push --tags"
    );
    assert_eq!(
        messages[8],
        "build-release skill: retired release route remains: .claude/skills/lumen-build-release/scripts/release.sh"
    );
}

#[test]
fn control_with_missing_anchor_returns_setup_error() {
    let controls = skill_controls("no anchors here");
    assert_eq!(
        refusal_text(controls[1].1()),
        "build-release skill: mutation anchor must appear exactly once: 'Publish one verified release' (0)"
    );
}
