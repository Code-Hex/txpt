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
fn shell_mode_runs_redirection_inside_child_shell() {
    let dir = TempDir::new().unwrap();
    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .args([
            "run",
            "--root",
            dir.path().to_str().unwrap(),
            "--snapshot",
            "copy",
            "--shell",
            "printf shell-output > redirected.txt",
        ])
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(dir.path().join("redirected.txt")).unwrap(),
        "shell-output"
    );
}

#[test]
fn shell_mode_can_expand_aliases_defined_in_shell_command() {
    let dir = TempDir::new().unwrap();
    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .env("SHELL", "/bin/sh")
        .args([
            "run",
            "--root",
            dir.path().to_str().unwrap(),
            "--snapshot",
            "copy",
            "--shell",
            "alias txpt_write='printf aliased > alias.txt'\ntxpt_write",
        ])
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(dir.path().join("alias.txt")).unwrap(),
        "aliased"
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
fn conflict_prevents_any_default_rollback() {
    let dir = TempDir::new().unwrap();
    let clean = dir.path().join("clean.txt");
    let conflicted = dir.path().join("conflicted.txt");
    fs::write(&clean, "before-clean\n").unwrap();
    fs::write(&conflicted, "before-conflict\n").unwrap();

    run_tx(
        dir.path(),
        "printf after-clean > clean.txt; printf after-conflict > conflicted.txt",
    )
    .success();
    fs::write(&conflicted, "manual\n").unwrap();

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().code(80);
    assert_eq!(fs::read_to_string(clean).unwrap(), "after-clean");
    assert_eq!(fs::read_to_string(conflicted).unwrap(), "manual\n");
}

#[test]
fn created_directory_with_extra_current_file_is_not_removed() {
    let dir = TempDir::new().unwrap();

    run_tx(dir.path(), "mkdir -p out && printf generated > out/a.txt").success();
    fs::write(dir.path().join("out/important.txt"), "do not delete\n").unwrap();

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().code(80);
    assert_eq!(
        fs::read_to_string(dir.path().join("out/important.txt")).unwrap(),
        "do not delete\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("out/a.txt")).unwrap(),
        "generated"
    );
}

#[test]
fn force_created_directory_conflict_preserves_current_subtree() {
    let dir = TempDir::new().unwrap();

    run_tx(dir.path(), "mkdir -p out && printf generated > out/a.txt").success();
    fs::write(dir.path().join("out/important.txt"), "do not delete\n").unwrap();

    let mut undo = txpt();
    undo.current_dir(dir.path())
        .args(["undo", "--force"])
        .assert()
        .success();

    assert!(!dir.path().join("out").exists());
    let conflicts = dir.path().join(".txpt/conflicts");
    let preserved = walkdir::WalkDir::new(conflicts)
        .into_iter()
        .filter_map(Result::ok)
        .any(|entry| {
            entry.file_name() == "important.txt"
                && fs::read_to_string(entry.path()).is_ok_and(|text| text == "do not delete\n")
        });
    assert!(preserved);
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
        .args(["diff", "--name-status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("!\tignored.txt"))
        .stdout(predicate::str::contains("!\t.env"));
}

#[test]
fn list_show_and_diff_surface_rollback_readiness() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("package.json"), "{}\n").unwrap();
    run_tx(dir.path(), "printf '{\"dependencies\":{}}' > package.json").success();

    let mut list = txpt();
    list.current_dir(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("@last"))
        .stdout(predicate::str::contains("undoable"))
        .stdout(predicate::str::contains("package.json"));

    let mut show = txpt();
    show.current_dir(dir.path())
        .args(["show", "@last"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rollback:"))
        .stdout(predicate::str::contains("state: Undoable"));

    let mut diff = txpt();
    diff.current_dir(dir.path())
        .args(["diff", "@last"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rollback:"))
        .stdout(predicate::str::contains("--- before/package.json"))
        .stdout(predicate::str::contains("+++ after/package.json"));
}

#[test]
fn list_tolerates_legacy_change_schema() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    run_tx(dir.path(), "printf b > a.txt").success();

    let tx_dir = only_tx_dir(dir.path());
    let changes_path = tx_dir.join("changes.jsonl");
    let legacy = fs::read_to_string(&changes_path).unwrap().replace(
        ",\"before_size\":2,\"after_size\":1,\"text_diff_available\":true",
        "",
    );
    fs::write(changes_path, legacy).unwrap();

    let mut list = txpt();
    list.current_dir(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("@last"))
        .stdout(predicate::str::contains("undoable"));
}

#[test]
fn list_marks_incomplete_transaction_as_broken() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".txpt/tx/9999999999-bad")).unwrap();

    let mut list = txpt();
    list.current_dir(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("broken"));
}

#[test]
fn spawn_failure_does_not_leave_latest_broken_transaction() {
    let dir = TempDir::new().unwrap();
    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .args([
            "run",
            "--root",
            dir.path().to_str().unwrap(),
            "--snapshot",
            "copy",
            "--",
            "txpt-command-does-not-exist",
        ])
        .assert()
        .failure();

    let tx_root = dir.path().join(".txpt/tx");
    let count = fs::read_dir(tx_root)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(count, 0);
}

#[test]
fn rollback_alias_uses_undo() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    run_tx(dir.path(), "printf b > a.txt").success();

    let mut rollback = txpt();
    rollback
        .current_dir(dir.path())
        .args(["rollback", "@last", "--dry-run"])
        .assert()
        .success()
        .stderr(predicate::str::contains("txpt undo plan"));
}

#[test]
fn diff_modes_are_available() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    run_tx(dir.path(), "printf b > a.txt").success();

    let mut stat = txpt();
    stat.current_dir(dir.path())
        .args(["diff", "--stat"])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"))
        .stdout(predicate::str::contains("before="));

    let mut name_status = txpt();
    name_status
        .current_dir(dir.path())
        .args(["diff", "--name-status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("M\ta.txt"));

    let mut json = txpt();
    json.current_dir(dir.path())
        .args(["diff", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"plan\""))
        .stdout(predicate::str::contains("\"changes\""));
}

fn only_tx_dir(root: &Path) -> std::path::PathBuf {
    fs::read_dir(root.join(".txpt/tx"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

#[test]
fn record_only_transaction_cannot_undo() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "before\n").unwrap();

    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .args([
            "run",
            "--root",
            dir.path().to_str().unwrap(),
            "--snapshot",
            "off",
            "--",
            "sh",
            "-c",
            "printf after > a.txt",
        ])
        .assert()
        .success();

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().code(81);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "after"
    );

    let mut diff = txpt();
    diff.current_dir(dir.path())
        .args(["diff", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("record_only"));
}

#[test]
fn sensitive_file_manifest_does_not_store_hash_by_default() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".env"), "SECRET=lowentropy\n").unwrap();
    run_tx(dir.path(), "printf SECRET=changed > .env").success();

    let tx_root = dir.path().join(".txpt/tx");
    let tx_dir = fs::read_dir(tx_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let before = fs::read_to_string(tx_dir.join("before.manifest.jsonl")).unwrap();
    let after = fs::read_to_string(tx_dir.join("after.manifest.jsonl")).unwrap();
    assert!(before.contains("\"path\":\".env\""));
    assert!(after.contains("\"path\":\".env\""));
    assert!(before.contains("\"hash\":null"));
    assert!(after.contains("\"hash\":null"));
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
