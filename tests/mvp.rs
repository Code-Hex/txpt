use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn txpt() -> Command {
    Command::cargo_bin("txpt").expect("txpt binary")
}

fn run_tx(root: &Path, script: &str) -> assert_cmd::assert::Assert {
    let mut cmd = txpt();
    cmd.current_dir(root);
    cmd.args([
        "run",
        "--root",
        root.to_str().expect("utf8 temp path"),
        "--snapshot",
        "copy",
        "--",
        "sh",
        "-c",
        script,
    ]);
    cmd.assert()
}

#[test]
fn modified_file_rolls_back() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("package.json");
    fs::write(&file, "before\n").unwrap();

    run_tx(dir.path(), "printf after > package.json").success();

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().success();
    assert_eq!(fs::read_to_string(file).unwrap(), "before\n");
}

#[test]
fn created_file_is_removed() {
    let dir = TempDir::new().unwrap();

    run_tx(dir.path(), "printf new > created.txt").success();

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().success();
    assert!(!dir.path().join("created.txt").exists());
}

#[test]
fn double_dash_is_run_shorthand() {
    let dir = TempDir::new().unwrap();
    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .args(["--", "sh", "-c", "printf ok > shorthand.txt"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("shorthand.txt")).unwrap(),
        "ok"
    );
}

#[test]
fn deleted_file_is_restored() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("gone.txt");
    fs::write(&file, "keep\n").unwrap();

    run_tx(dir.path(), "rm gone.txt").success();

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().success();
    assert_eq!(fs::read_to_string(file).unwrap(), "keep\n");
}

#[test]
fn conflict_does_not_overwrite_without_force() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("package.json");
    fs::write(&file, "before\n").unwrap();
    run_tx(dir.path(), "printf after > package.json").success();
    fs::write(&file, "manual\n").unwrap();

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().code(80);
    assert_eq!(fs::read_to_string(file).unwrap(), "manual\n");
}

#[test]
fn force_conflict_preserves_current_and_restores() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("package.json");
    fs::write(&file, "before\n").unwrap();
    run_tx(dir.path(), "printf after > package.json").success();
    fs::write(&file, "manual\n").unwrap();

    let mut undo = txpt();
    undo.current_dir(dir.path())
        .args(["undo", "--force"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&file).unwrap(), "before\n");
    let conflicts = dir.path().join(".txpt/conflicts");
    let preserved = walkdir::WalkDir::new(conflicts)
        .into_iter()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name() == "package.json.current");
    assert!(preserved);
}

#[test]
fn child_exit_code_is_preserved_and_transaction_recorded() {
    let dir = TempDir::new().unwrap();

    run_tx(dir.path(), "printf changed > file.txt; exit 7").code(7);

    assert!(dir.path().join(".txpt/tx").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "changed"
    );
}

#[test]
fn ignored_and_sensitive_changes_are_unprotected_by_default() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(dir.path().join("ignored.txt"), "before\n").unwrap();
    fs::write(dir.path().join(".env"), "SECRET=before\n").unwrap();

    run_tx(
        dir.path(),
        "printf after > ignored.txt; printf SECRET=after > .env",
    )
    .success();

    let mut diff = txpt();
    diff.current_dir(dir.path())
        .arg("diff")
        .assert()
        .success()
        .stdout(predicate::str::contains("unprotected").count(2));
}

#[test]
fn inspect_json_reports_capabilities() {
    let mut cmd = txpt();
    cmd.args(["inspect", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"name\": \"txpt\""))
        .stdout(predicate::str::contains("\"undo\": true"));
}
