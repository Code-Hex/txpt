use std::fs;
use std::os::unix::fs::PermissionsExt;
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
fn unknown_subcommand_is_usage_error_not_implicit_run() {
    let dir = TempDir::new().unwrap();
    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .arg("dpctpr")
        .assert()
        .code(64)
        .stderr(predicate::str::contains("unknown subcommand dpctpr"))
        .stderr(predicate::str::contains("usage:"))
        .stderr(predicate::str::contains("txpt run [options] -- <cmd>"));

    let tx_root = dir.path().join(".txpt/tx");
    assert!(!tx_root.exists());
}

#[test]
fn top_level_double_dash_is_run_shorthand() {
    let dir = TempDir::new().unwrap();
    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .args(["--", "sh", "-c", "printf ok > file.txt"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "ok"
    );
}

#[test]
fn no_args_non_tty_prints_help() {
    let mut cmd = txpt();
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("protected shell session"))
        .stderr(predicate::str::contains("txpt -- <cmd>"));
}

#[test]
fn shim_exec_wraps_non_ignored_command_and_records_display_argv() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    let session_bin = session.join("bin");
    let real_bin = dir.path().join("real-bin");
    fs::create_dir_all(&session_bin).unwrap();
    fs::create_dir_all(&real_bin).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#,
    )
    .unwrap();
    write_executable(
        &real_bin.join("npm"),
        "#!/bin/sh\nprintf '{\"dependencies\":{\"zod\":\"1\"}}' > package.json\n",
    );

    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                session_bin.display(),
                real_bin.display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["shim-exec", "npm", "install", "zod"])
        .assert()
        .success();

    let tx_dir = only_tx_dir(dir.path());
    let command_json = fs::read_to_string(tx_dir.join("command.json")).unwrap();
    assert!(command_json.contains(r#""argv": ["#));
    assert!(command_json.contains(r#""npm""#));
    assert!(command_json.contains(r#""real_argv": ["#));
    assert!(command_json.contains(real_bin.join("npm").to_str().unwrap()));
}

#[test]
fn shim_exec_wraps_ignored_outputs_so_npm_install_can_undo_node_modules() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    let session_bin = session.join("bin");
    let real_bin = dir.path().join("real-bin");
    fs::create_dir_all(&session_bin).unwrap();
    fs::create_dir_all(&real_bin).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#,
    )
    .unwrap();
    write_executable(
        &real_bin.join("npm"),
        "#!/bin/sh\nmkdir -p node_modules/zod\nprintf zod > node_modules/zod/index.js\nprintf '{\"dependencies\":{\"zod\":\"1\"}}' > package.json\n",
    );

    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                session_bin.display(),
                real_bin.display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["shim-exec", "npm", "install", "zod"])
        .assert()
        .success();
    assert!(dir.path().join("node_modules/zod/index.js").exists());

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().success();
    assert!(!dir.path().join("node_modules").exists());
    assert!(!dir.path().join("package.json").exists());
}

#[test]
fn shim_exec_wraps_rm_so_deleted_files_can_undo_in_session() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    let session_bin = session.join("bin");
    let real_bin = dir.path().join("real-bin");
    fs::create_dir_all(&session_bin).unwrap();
    fs::create_dir_all(&real_bin).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["rm:*"],"ignore":[]}"#,
    )
    .unwrap();
    fs::write(dir.path().join("keep.txt"), "keep\n").unwrap();
    write_executable(&real_bin.join("rm"), "#!/bin/sh\n/bin/rm \"$@\"\n");

    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                session_bin.display(),
                real_bin.display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["shim-exec", "rm", "keep.txt"])
        .assert()
        .success();
    assert!(!dir.path().join("keep.txt").exists());

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().success();
    assert_eq!(
        fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
        "keep\n"
    );
}

#[test]
fn shim_exec_passes_through_ignored_pattern() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    let session_bin = session.join("bin");
    let real_bin = dir.path().join("real-bin");
    fs::create_dir_all(&session_bin).unwrap();
    fs::create_dir_all(&real_bin).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#,
    )
    .unwrap();
    write_executable(&real_bin.join("npm"), "#!/bin/sh\nprintf test > ran.txt\n");

    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                session_bin.display(),
                real_bin.display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["shim-exec", "npm", "test"])
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(dir.path().join("ran.txt")).unwrap(),
        "test"
    );
    assert!(!dir.path().join(".txpt/tx").exists());
}

#[test]
fn shims_are_generated_from_protect_rules() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    let real_bin = dir.path().join("real-bin");
    fs::create_dir_all(session.join("bin")).unwrap();
    fs::create_dir_all(&real_bin).unwrap();
    fs::write(session.join("policy.json"), r#"{"protect":[],"ignore":[]}"#).unwrap();
    write_executable(&real_bin.join("custom-mutator"), "#!/bin/sh\nexit 0\n");

    let mut protect = txpt();
    protect
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .env(
            "PATH",
            format!("{}:{}", real_bin.display(), std::env::var("PATH").unwrap()),
        )
        .args(["shims", "protect", "custom-mutator:*"])
        .assert()
        .success();

    assert!(session.join("bin/custom-mutator").exists());
}

#[test]
fn shim_should_wrap_and_argv_json_support_shell_function_wrappers() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#,
    )
    .unwrap();

    let mut should_wrap = txpt();
    should_wrap
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "npm", "install", "zod"])
        .assert()
        .success();

    let mut should_not_wrap = txpt();
    should_not_wrap
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "npm", "test"])
        .assert()
        .code(1);

    let mut json = txpt();
    json.current_dir(dir.path())
        .args(["argv-json", "npm", "install", "zod"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#"["npm","install","zod"]"#));
}

#[test]
fn shim_policy_uses_claude_bash_pattern_syntax() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm run *","cargo update:*","* --version"],"ignore":["npm test:*","cargo test:*"]}"#,
    )
    .unwrap();

    let mut npm_run = txpt();
    npm_run
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "npm", "run", "build"])
        .assert()
        .success();

    let mut cargo_update = txpt();
    cargo_update
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "cargo", "update"])
        .assert()
        .success();

    let mut version = txpt();
    version
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "npm", "--version"])
        .assert()
        .success();

    let mut lsof_boundary = txpt();
    lsof_boundary
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "npm", "test"])
        .assert()
        .code(1);
}

#[test]
fn shim_policy_matches_destructive_workspace_command_patterns() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["unlink:*","ln:*","truncate:*","patch:*","tee:*","rsync:*","dd:*","sed:* -i:*","sed:* --in-place:*","perl:* -i:*","find:* -delete:*","find:* -exec rm:*","find:* -execdir rm:*","xargs:* rm:*","git clean:*","git reset --hard:*","git checkout --:*","git restore:*","git rm:*","git apply:*","git stash pop:*","git stash apply:*","uv add:*","uv remove:*","uv sync:*","uv lock:*"],"ignore":["* --version"]}"#,
    )
    .unwrap();

    for args in [
        vec!["shim-should-wrap", "unlink", "old.txt"],
        vec!["shim-should-wrap", "ln", "-s", "a", "b"],
        vec!["shim-should-wrap", "truncate", "-s", "0", "file.txt"],
        vec!["shim-should-wrap", "patch", "-p1"],
        vec!["shim-should-wrap", "tee", "file.txt"],
        vec!["shim-should-wrap", "rsync", "-a", "src/", "dst/"],
        vec!["shim-should-wrap", "dd", "if=a", "of=b"],
        vec!["shim-should-wrap", "sed", "-i", "s/a/b/", "file.txt"],
        vec![
            "shim-should-wrap",
            "sed",
            "s/a/b/",
            "--in-place",
            "file.txt",
        ],
        vec![
            "shim-should-wrap",
            "perl",
            "-i",
            "-pe",
            "s/a/b/",
            "file.txt",
        ],
        vec!["shim-should-wrap", "find", ".", "-delete"],
        vec!["shim-should-wrap", "find", ".", "-exec", "rm", "{}", ";"],
        vec!["shim-should-wrap", "find", ".", "-execdir", "rm", "{}", ";"],
        vec!["shim-should-wrap", "xargs", "-0", "rm", "-f"],
        vec!["shim-should-wrap", "git", "clean", "-fd"],
        vec!["shim-should-wrap", "git", "reset", "--hard"],
        vec!["shim-should-wrap", "git", "checkout", "--", "file.txt"],
        vec!["shim-should-wrap", "git", "restore", "file.txt"],
        vec!["shim-should-wrap", "git", "rm", "file.txt"],
        vec!["shim-should-wrap", "git", "apply", "change.patch"],
        vec!["shim-should-wrap", "git", "stash", "pop"],
        vec!["shim-should-wrap", "git", "stash", "apply"],
        vec!["shim-should-wrap", "uv", "add", "requests"],
        vec!["shim-should-wrap", "uv", "remove", "requests"],
        vec!["shim-should-wrap", "uv", "sync"],
        vec!["shim-should-wrap", "uv", "lock"],
    ] {
        let mut cmd = txpt();
        cmd.current_dir(dir.path())
            .env("TXPT_SESSION_DIR", &session)
            .args(args)
            .assert()
            .success();
    }

    let mut version = txpt();
    version
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "git", "--version"])
        .assert()
        .code(1);
}

#[test]
fn shim_policy_ignores_command_metadata_checks() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["git:*"],"ignore":["* --help","* -h","* --version","* -v","* version","* help"]}"#,
    )
    .unwrap();

    for args in [
        vec!["shim-should-wrap", "git", "--help"],
        vec!["shim-should-wrap", "git", "-h"],
        vec!["shim-should-wrap", "git", "--version"],
        vec!["shim-should-wrap", "git", "-v"],
        vec!["shim-should-wrap", "git", "version"],
        vec!["shim-should-wrap", "git", "help"],
    ] {
        let mut cmd = txpt();
        cmd.current_dir(dir.path())
            .env("TXPT_SESSION_DIR", &session)
            .args(args)
            .assert()
            .code(1);
    }

    let mut destructive = txpt();
    destructive
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "git", "clean", "-fd"])
        .assert()
        .success();
}

#[test]
fn shim_begin_finish_records_current_shell_command_without_spawning_child() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();

    let mut begin = txpt();
    let output = begin
        .current_dir(dir.path())
        .args(["shim-begin", r#"["npm","install","zod"]"#])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(output).unwrap().trim().to_owned();

    fs::create_dir_all(dir.path().join("node_modules/zod")).unwrap();
    fs::write(dir.path().join("node_modules/zod/index.js"), "zod").unwrap();
    fs::write(dir.path().join("package-lock.json"), "lock").unwrap();

    let mut finish = txpt();
    finish
        .current_dir(dir.path())
        .args(["shim-finish", &id, "0"])
        .assert()
        .success()
        .stderr(predicate::str::contains("command:\n  npm install zod"));

    let command_json = fs::read_to_string(
        dir.path()
            .join(".txpt")
            .join("tx")
            .join(&id)
            .join("command.json"),
    )
    .unwrap();
    assert!(command_json.contains(r#""shell": "txpt-session-function""#));

    let mut undo = txpt();
    undo.current_dir(dir.path()).arg("undo").assert().success();
    assert!(!dir.path().join("node_modules").exists());
    assert!(!dir.path().join("package-lock.json").exists());
}

#[test]
fn shims_commands_update_active_policy_and_scripts() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(session.join("bin")).unwrap();
    fs::write(session.join("policy.json"), r#"{"protect":[],"ignore":[]}"#).unwrap();

    let mut ignore = txpt();
    ignore
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shims", "protect", "npm install:*"])
        .assert()
        .success();
    assert!(session.join("bin/npm").exists());

    let mut ignore = txpt();
    ignore
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shims", "ignore", "npm test *"])
        .assert()
        .success();

    let mut list = txpt();
    list.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shims", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("shims:"))
        .stdout(predicate::str::contains("protect:"))
        .stdout(predicate::str::contains("ignore:"))
        .stdout(predicate::str::contains("npm install:*"))
        .stdout(predicate::str::contains("npm test *"));

    let mut unprotect = txpt();
    unprotect
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shims", "unprotect", "npm install:*"])
        .assert()
        .success();

    let mut unignore = txpt();
    unignore
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shims", "unignore", "npm test *"])
        .assert()
        .success();
    let policy = fs::read_to_string(session.join("policy.json")).unwrap();
    assert!(!policy.contains("npm install:*"));
    assert!(!policy.contains("npm test *"));
}

#[test]
fn ls_aliases_match_list_commands() {
    let dir = TempDir::new().unwrap();
    run_tx(dir.path(), "printf after > file.txt").success();

    let mut top = txpt();
    top.current_dir(dir.path())
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("@last"));

    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#,
    )
    .unwrap();

    let mut shims = txpt();
    shims
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shims", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ignore:"));
}

#[test]
fn shims_edit_opens_editor_for_active_policy() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(session.join("bin")).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#,
    )
    .unwrap();

    let mut edit = txpt();
    edit.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .env("EDITOR", "true")
        .args(["shims", "edit"])
        .assert()
        .success();
}

#[test]
fn no_args_inside_session_prints_dashboard() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(session.join("bin")).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#,
    )
    .unwrap();

    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .env("TXPT_ACTIVE", "1")
        .env("TXPT_SESSION_DIR", &session)
        .assert()
        .success()
        .stdout(predicate::str::contains("txpt session"))
        .stdout(predicate::str::contains("policy:"))
        .stdout(predicate::str::contains("npm"));
}

#[test]
fn run_refuses_nested_txpt_session_inside_session() {
    let dir = TempDir::new().unwrap();
    let mut cmd = txpt();
    cmd.current_dir(dir.path())
        .env("TXPT_ACTIVE", "1")
        .args(["run", "--", "txpt"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refusing nested txpt session"));
    assert!(!dir.path().join(".txpt/tx").exists());
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
        .stdout(predicate::str::contains("state: undoable"))
        .stdout(predicate::str::contains("txpt diff"));

    let mut diff = txpt();
    diff.current_dir(dir.path())
        .args(["diff", "@last"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rollback:"))
        .stdout(predicate::str::contains("--- before/package.json"))
        .stdout(predicate::str::contains("+++ after/package.json"))
        .stdout(predicate::str::contains("@@"));
}

#[test]
fn show_selector_title_and_commands_use_stable_id() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    run_tx(dir.path(), "printf b > a.txt").success();
    std::thread::sleep(std::time::Duration::from_millis(10));
    run_tx(dir.path(), "printf c > a.txt").success();
    let mut ids = tx_ids(dir.path());
    ids.sort();
    let older_id = ids.first().unwrap().clone();

    let mut show = txpt();
    show.current_dir(dir.path())
        .args(["show", "@1"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("txpt @1  {older_id}")))
        .stdout(predicate::str::contains(format!("txpt diff {older_id}")))
        .stdout(predicate::str::contains(format!(
            "txpt undo {older_id} --dry-run"
        )));
}

#[test]
fn diff_uses_unified_hunks_and_binary_summary() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("package.json"),
        "{\n  \"name\": \"app\"\n}\n",
    )
    .unwrap();
    run_tx(
        dir.path(),
        "printf '{\\n  \"name\": \"app\",\\n  \"dependencies\": {\\n    \"zod\": \"1\"\\n  }\\n}\\n' > package.json",
    )
    .success();

    let mut diff = txpt();
    diff.current_dir(dir.path())
        .args(["diff", "@last"])
        .assert()
        .success()
        .stdout(predicate::str::contains("@@"))
        .stdout(predicate::str::contains("-  \"name\": \"app\""))
        .stdout(predicate::str::contains("+  \"name\": \"app\","));

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("bin.dat"), [0xff, 0x00, 0x41]).unwrap();
    run_tx(dir.path(), "printf '\\xfe\\x00B' > bin.dat").success();
    let mut binary_diff = txpt();
    binary_diff
        .current_dir(dir.path())
        .args(["diff", "@last"])
        .assert()
        .success()
        .stdout(predicate::str::contains("binary or non-text change"))
        .stdout(predicate::str::contains("hash="));
}

#[test]
fn apply_plan_revalidates_before_modifying() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("a.txt");
    fs::write(&file, "before\n").unwrap();
    run_tx(dir.path(), "printf after > a.txt").success();

    let paths = txpt::storage::existing_tx_paths(dir.path(), None).unwrap();
    let plan = txpt::rollback::plan(dir.path(), &paths);
    assert_eq!(plan.state, txpt::rollback::TxState::Undoable);
    fs::write(&file, "manual\n").unwrap();

    let err = txpt::rollback::apply_plan(dir.path(), &paths, &plan, false, false).unwrap_err();
    assert!(err.to_string().contains("rollback conflict"));
    assert_eq!(fs::read_to_string(file).unwrap(), "manual\n");
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

fn tx_ids(root: &Path) -> Vec<String> {
    fs::read_dir(root.join(".txpt/tx"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
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

    let mut list = txpt();
    list.current_dir(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("record-only"));
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
