//! Tests for the table checks: `apps.sh` against the APPS table and the Rust
//! receipt code. Each refusal asserts the full text with `assert_eq!`.

use keep_release_gate::contract::{find_app, App, ContractError};
use keep_release_gate::tables_contract::{
    bash_predicate, bash_values, check_constants, check_receipt_rows, check_tables, receipt_rows,
    ReceiptRow,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn keep() -> App {
    find_app("keep").unwrap()
}

/// The refusal text (the inner message, without any prefix).
fn refused<T: std::fmt::Debug>(result: Result<T, ContractError>) -> String {
    match result {
        Err(error) => error.0,
        Ok(value) => panic!("expected a refusal, got {value:?}"),
    }
}

/// A temporary repository root whose `scripts/release/apps.sh` is the real
/// file with the first `from` replaced by `to`.
fn mutated(from: &str, to: &str) -> tempfile::TempDir {
    let text = std::fs::read_to_string(root().join("scripts/release/apps.sh")).unwrap();
    assert_eq!(
        text.matches(from).count(),
        1,
        "anchor {from:?} must occur once"
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("scripts/release")).unwrap();
    std::fs::write(
        dir.path().join("scripts/release/apps.sh"),
        text.replacen(from, to, 1),
    )
    .unwrap();
    dir
}

#[test]
fn real_tables_pass() {
    assert_eq!(check_tables(&root(), &[keep()], &receipt_rows()), Ok(()));
}

#[test]
fn target_list_mismatch() {
    let dir = mutated(
        "printf '%s\\n' x86_64-unknown-linux-musl aarch64-unknown-linux-musl",
        "printf '%s\\n' x86_64-unknown-linux-musl",
    );
    assert_eq!(
        refused(check_tables(dir.path(), &[keep()], &receipt_rows())),
        "apps.sh release_app_targets keep = ['x86_64-unknown-linux-musl'], this table says ['x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl']"
    );
}

#[test]
fn receipt_name_mismatch() {
    let dir = mutated(
        "printf '%s-gke-receipt.json\\n' \"$1\"",
        "printf '%s-receipt.json\\n' \"$1\"",
    );
    assert_eq!(
        refused(check_tables(dir.path(), &[keep()], &receipt_rows())),
        "apps.sh release_app_receipt_name keep = ['keep-receipt.json'], this table says ['keep-gke-receipt.json']"
    );
}

#[test]
fn loom_known_is_refused() {
    let dir = mutated(
        "case \"$1\" in keep) return 0 ;;",
        "case \"$1\" in keep|loom) return 0 ;;",
    );
    assert_eq!(
        refused(check_tables(dir.path(), &[keep()], &receipt_rows())),
        "apps.sh must not know loom: it has no candidate or promotion"
    );
}

#[test]
fn shared_scripts_flag_mismatch() {
    let mut app = keep();
    app.shared_scripts = false;
    assert_eq!(
        refused(check_tables(&root(), &[app], &receipt_rows())),
        "apps.sh disagrees on whether keep uses the shared scripts"
    );
}

#[test]
fn takes_attempt_mismatch() {
    let mut app = keep();
    app.takes_attempt = false;
    assert_eq!(
        refused(check_tables(&root(), &[app], &receipt_rows())),
        "apps.sh disagrees on whether keep promotion takes a run attempt"
    );
}

#[test]
fn rust_row_target_mismatch() {
    let mut rows = receipt_rows();
    rows.get_mut("keep").unwrap().targets.truncate(1);
    assert_eq!(
        refused(check_tables(&root(), &[keep()], &rows)),
        "receipt maker targets for keep differ from this table"
    );
}

#[test]
fn rows_name_set_mismatch() {
    assert_eq!(
        refused(check_receipt_rows(&[keep()], &BTreeMap::new())),
        "receipt maker knows [], shared-script apps are ['keep']"
    );
}

#[test]
fn receipt_rows_other_mismatches() {
    let rows = receipt_rows();
    assert_eq!(check_receipt_rows(&[keep()], &rows), Ok(()));

    let mut bad_row: BTreeMap<String, ReceiptRow> = rows.clone();
    bad_row.get_mut("keep").unwrap().backend = "gcp".to_string();
    assert_eq!(
        refused(check_receipt_rows(&[keep()], &bad_row)),
        "receipt maker backend for keep is gcp, table says gke-acceptance"
    );

    let mut bad_row: BTreeMap<String, ReceiptRow> = rows.clone();
    bad_row.get_mut("keep").unwrap().functional = vec!["readyz".to_string()];
    assert_eq!(
        refused(check_receipt_rows(&[keep()], &bad_row)),
        "receipt maker functional fields for keep differ from this table"
    );

    let mut app = keep();
    app.manifest_schema = "x".to_string();
    assert_eq!(
        refused(check_receipt_rows(&[app], &rows)),
        "receipt maker candidate schema for keep differs from this table"
    );

    let mut app = keep();
    app.receipt_schema = "x".to_string();
    assert_eq!(
        refused(check_receipt_rows(&[app], &rows)),
        "receipt maker receipt schema for keep differs from this table"
    );

    let mut app = keep();
    app.candidate_jobs.swap(0, 1);
    assert_eq!(
        refused(check_receipt_rows(&[app], &rows)),
        "receipt maker job inventory for keep differs from this table"
    );
}

#[test]
fn constants_mismatch() {
    assert_eq!(
        refused(check_constants(&[("REPOSITORY", "a", "b")])),
        "receipt maker REPOSITORY = 'a', expected 'b'"
    );
    assert_eq!(check_constants(&[("X", "a", "a")]), Ok(()));
}

#[test]
fn bash_values_and_predicate_refuse_unknown_app() {
    assert_eq!(
        refused(bash_values(&root(), "release_app_root", "loom")),
        "apps.sh release_app_root loom: exit 2: refused: unknown release app: loom (known: keep)"
    );
    assert_eq!(
        refused(bash_predicate(&root(), "release_app_require", "loom")),
        "apps.sh release_app_require loom: exit 2: refused: unknown release app: loom (known: keep)"
    );
    assert_eq!(
        bash_predicate(&root(), "release_app_known", "loom"),
        Ok(false)
    );
    assert_eq!(
        bash_predicate(&root(), "release_app_known", "keep"),
        Ok(true)
    );
    assert_eq!(
        bash_values(&root(), "release_app_root", "keep"),
        Ok(vec![".".to_string()])
    );
}

#[test]
fn bash_start_failure_refused() {
    let text = refused(bash_values(
        Path::new("/nonexistent-root"),
        "release_app_root",
        "keep",
    ));
    assert!(
        text.starts_with("apps.sh release_app_root keep: exit "),
        "unexpected text: {text}"
    );
}
