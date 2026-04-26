#![allow(unsafe_code)]

use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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

fn run_with_display_command(root: &Path, display_json: &str, script: &str) -> String {
    let mut cmd = txpt();
    let output = cmd
        .current_dir(root)
        .args([
            "run",
            "--root",
            root.to_str().expect("utf8 temp path"),
            "--snapshot",
            "copy",
            "--display-command",
            display_json,
            "--",
            "sh",
            "-c",
            script,
        ])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
}

fn run_interactive_txpt_session(
    root: &Path,
    extra_path: Option<&Path>,
    input: &str,
) -> std::process::Output {
    run_interactive_txpt_session_with_env(root, "/bin/sh", extra_path, &[], input)
}

fn run_interactive_txpt_session_with_env(
    root: &Path,
    shell: &str,
    extra_path: Option<&Path>,
    extra_env: &[(&str, &str)],
    input: &str,
) -> std::process::Output {
    let txpt_bin = assert_cmd::cargo::cargo_bin("txpt");
    let mut master = 0;
    let mut slave = 0;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");

    let master_file = unsafe { fs::File::from_raw_fd(master) };
    let mut writer = master_file.try_clone().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader_output = Arc::clone(&output);
    thread::spawn(move || {
        let mut reader = master_file;
        let mut chunk = [0; 8192];
        loop {
            let read = reader.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            reader_output
                .lock()
                .unwrap()
                .extend_from_slice(&chunk[..read]);
        }
    });

    let slave_file = unsafe { fs::File::from_raw_fd(slave) };
    let mut command = StdCommand::new(txpt_bin);
    command.current_dir(root).env("SHELL", shell);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    if let Some(extra_path) = extra_path {
        command.env(
            "PATH",
            format!(
                "{}:{}",
                extra_path.display(),
                std::env::var("PATH").unwrap()
            ),
        );
    }
    let mut child = command
        .stdin(Stdio::from(slave_file.try_clone().unwrap()))
        .stdout(Stdio::from(slave_file.try_clone().unwrap()))
        .stderr(Stdio::from(slave_file))
        .spawn()
        .expect("start pty-driven txpt session");
    thread::sleep(Duration::from_millis(300));
    writer
        .write_all(input.as_bytes())
        .expect("write session input");
    drop(writer);
    let status = child.wait().expect("wait for txpt session");
    thread::sleep(Duration::from_millis(100));
    std::process::Output {
        status,
        stdout: output.lock().unwrap().clone(),
        stderr: Vec::new(),
    }
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
fn protected_shell_session_e2e_wraps_commands_and_undoes_points() {
    let dir = TempDir::new().unwrap();
    let real_bin = dir.path().join("real-bin");
    fs::create_dir_all(&real_bin).unwrap();
    fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
    fs::write(dir.path().join("doomed.txt"), "important\n").unwrap();
    write_executable(
        &real_bin.join("npm"),
        "#!/bin/sh\npython3 - <<'PY'\nimport os\nos.makedirs('node_modules/zod', exist_ok=True)\nopen('node_modules/zod/index.js', 'w').write('zod')\nopen('package.json', 'w').write('{\"dependencies\":{\"zod\":\"1\"}}')\nPY\n",
    );

    let input = "rm doomed.txt\ntxpt diff @last\ntxpt undo @last\nhash -r\nnpm install zod\ntxpt diff @last\ntxpt undo @last\nexit\n";
    let output = run_interactive_txpt_session(dir.path(), Some(&real_bin), input);
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        fs::read_to_string(dir.path().join("doomed.txt")).unwrap(),
        "important\n"
    );
    assert!(!dir.path().join("node_modules").exists());
    assert!(!dir.path().join("package.json").exists());
    let output = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.contains("command:\r\n  rm doomed.txt")
            || output.contains("command:\n  rm doomed.txt")
    );
    assert!(
        output.contains("command:\r\n  npm install zod")
            || output.contains("command:\n  npm install zod")
    );
    assert!(tx_ids(dir.path()).is_empty());
}

#[test]
fn shell_startup_commands_do_not_create_points() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join(".bashrc"),
        "mkdir -p startup-created\nprintf startup-ready\\n\n",
    )
    .unwrap();

    let output = run_interactive_txpt_session_with_env(
        dir.path(),
        "/bin/bash",
        None,
        &[("HOME", home.to_str().unwrap())],
        "mkdir user-created\ntxpt undo\nexit\n",
    );
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let output = String::from_utf8_lossy(&output.stdout);
    assert!(output.contains("startup-ready"));
    assert!(
        output.contains("command:\r\n  mkdir user-created")
            || output.contains("command:\n  mkdir user-created")
    );
    assert!(dir.path().join("startup-created").exists());
    assert!(!dir.path().join("user-created").exists());
    assert!(tx_ids(dir.path()).is_empty());
}

#[test]
fn run_receipt_warns_for_commands_with_partial_external_scope() {
    let dir = TempDir::new().unwrap();

    for (display, script) in [
        (r#"["dd","of=outside.img"]"#, "printf dd > dd.txt"),
        (
            r#"["rsync","-a","src/","dst/"]"#,
            "printf rsync > rsync.txt",
        ),
    ] {
        let stderr = run_with_display_command(dir.path(), display, script);
        assert!(stderr.contains("\nwarning:\n"), "{stderr}");
        assert!(
            stderr.contains(
                "command may modify files outside the transaction root; txpt only restores protected workspace paths."
            ),
            "{stderr}"
        );
    }

    for (display, script) in [
        (r#"["git","stash","pop"]"#, "printf stash > git-stash.txt"),
        (
            r#"["git","restore","--staged","file.txt"]"#,
            "printf staged > git-staged.txt",
        ),
        (
            r#"["git","rm","--cached","file.txt"]"#,
            "printf cached > git-cached.txt",
        ),
    ] {
        let stderr = run_with_display_command(dir.path(), display, script);
        assert!(stderr.contains("\nwarning:\n"), "{stderr}");
        assert!(
            stderr.contains(
                "git command may modify .git state; txpt restores workspace files, not Git index, reflog, stash, or repository metadata."
            ),
            "{stderr}"
        );
    }
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
        r#"{"protect":["unlink:*","ln:*","truncate:*","patch:*","tee:*","rsync:*","dd:*","sed:* -i:*","sed:* --in-place:*","perl:* -i:*","find:* -delete:*","find:* -exec rm:*","find:* -execdir rm:*","git clean:*","git reset --hard:*","git checkout --:*","git restore:*","git rm:*","git apply:*","git stash pop:*","git stash apply:*","uv add:*","uv remove:*","uv sync:*","uv lock:*"],"ignore":["* --version"]}"#,
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
        r#"{"protect":["git:*"],"ignore":["* --help","* -h","* --version","* -v"]}"#,
    )
    .unwrap();

    for args in [
        vec!["shim-should-wrap", "git", "--help"],
        vec!["shim-should-wrap", "git", "-h"],
        vec!["shim-should-wrap", "git", "--version"],
        vec!["shim-should-wrap", "git", "-v"],
    ] {
        let mut cmd = txpt();
        cmd.current_dir(dir.path())
            .env("TXPT_SESSION_DIR", &session)
            .args(args)
            .assert()
            .code(1);
    }

    for args in [
        vec!["shim-should-wrap", "git", "version"],
        vec!["shim-should-wrap", "git", "help"],
    ] {
        let mut cmd = txpt();
        cmd.current_dir(dir.path())
            .env("TXPT_SESSION_DIR", &session)
            .args(args)
            .assert()
            .success();
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
fn shims_edit_opens_editor_and_regenerates_shims() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(session.join("bin")).unwrap();
    fs::write(
        session.join("policy.json"),
        r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#,
    )
    .unwrap();
    assert!(!session.join("bin/npm").exists());

    let mut edit = txpt();
    edit.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .env("EDITOR", "true")
        .args(["shims", "edit"])
        .assert()
        .success();
    assert!(session.join("bin/npm").exists());
}

#[test]
fn shims_edit_fails_clearly_without_active_session() {
    let dir = TempDir::new().unwrap();
    let mut edit = txpt();
    edit.current_dir(dir.path())
        .env("EDITOR", "true")
        .args(["shims", "edit"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("TXPT_SESSION_DIR"))
        .stderr(predicate::str::contains("active txpt session"));
}

#[test]
fn shims_edit_reports_editor_failure() {
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
        .env("EDITOR", "false")
        .args(["shims", "edit"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("editor exited"));
}

#[test]
fn shims_edit_warns_on_invalid_policy_json() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join(".txpt/sessions/test");
    fs::create_dir_all(session.join("bin")).unwrap();
    let original_policy = r#"{"protect":["npm install:*"],"ignore":["npm test:*"]}"#;
    fs::write(session.join("policy.json"), original_policy).unwrap();
    write_executable(&session.join("bin/npm"), "#!/bin/sh\nexit 99\n");
    let original_shim = fs::read_to_string(session.join("bin/npm")).unwrap();
    let editor = dir.path().join("bad-editor");
    write_executable(&editor, "#!/bin/sh\nprintf '{not json' > \"$1\"\n");

    let mut edit = txpt();
    edit.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .env("EDITOR", &editor)
        .args(["shims", "edit"])
        .assert()
        .code(74)
        .stderr(predicate::str::contains("warning:"))
        .stderr(predicate::str::contains("policy.json is invalid"))
        .stderr(predicate::str::contains("live policy was left unchanged"))
        .stderr(predicate::str::contains("policy.json.edit-"))
        .stderr(predicate::str::contains("shims were not regenerated"));
    assert_eq!(
        fs::read_to_string(session.join("policy.json")).unwrap(),
        original_policy
    );
    let edit_paths = fs::read_dir(&session)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("policy.json.edit-"))
                .then_some(path)
        })
        .collect::<Vec<_>>();
    assert_eq!(edit_paths.len(), 1);
    assert_eq!(fs::read_to_string(&edit_paths[0]).unwrap(), "{not json");
    assert_eq!(
        fs::read_to_string(session.join("bin/npm")).unwrap(),
        original_shim
    );

    let mut list = txpt();
    list.current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shims", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("npm install:*"))
        .stdout(predicate::str::contains("npm test:*"));

    let mut should_wrap = txpt();
    should_wrap
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "npm", "install", "zod"])
        .assert()
        .success();

    let mut ignored = txpt();
    ignored
        .current_dir(dir.path())
        .env("TXPT_SESSION_DIR", &session)
        .args(["shim-should-wrap", "npm", "test"])
        .assert()
        .code(1);
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
fn reverted_points_are_popped_from_default_stack() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("first.txt"), "before\n").unwrap();
    run_tx(dir.path(), "printf first-after > first.txt").success();
    std::thread::sleep(std::time::Duration::from_millis(10));
    run_tx(dir.path(), "printf second > second.txt").success();
    let initial_ids = txpt::storage::list_tx_ids(dir.path()).unwrap();
    assert_eq!(initial_ids.len(), 2);
    let latest_id = initial_ids[0].clone();
    let previous_id = initial_ids[1].clone();
    assert!(dir.path().join(".txpt/tx").join(&latest_id).exists());
    assert!(dir.path().join(".txpt/tx").join(&previous_id).exists());

    let mut undo_latest = txpt();
    undo_latest
        .current_dir(dir.path())
        .arg("undo")
        .assert()
        .success();
    assert!(!dir.path().join(".txpt/tx").join(&latest_id).exists());
    assert!(dir.path().join(".txpt/tx").join(&previous_id).exists());
    assert_eq!(
        txpt::storage::resolve_tx_id(dir.path(), None).unwrap(),
        previous_id
    );
    assert_eq!(
        txpt::storage::list_tx_ids(dir.path()).unwrap(),
        vec![previous_id.clone()]
    );

    let mut list = txpt();
    list.current_dir(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("@last"))
        .stdout(predicate::str::contains("first.txt"))
        .stdout(predicate::str::contains("second.txt").not())
        .stdout(predicate::str::contains("reverted").not());

    let mut show = txpt();
    show.current_dir(dir.path())
        .args(["show", "@last"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "txpt @last  {previous_id}"
        )))
        .stdout(predicate::str::contains("first.txt"))
        .stdout(predicate::str::contains("state: undoable"));

    let mut undo_next = txpt();
    undo_next
        .current_dir(dir.path())
        .arg("undo")
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("first.txt")).unwrap(),
        "before\n"
    );
    assert!(!dir.path().join("second.txt").exists());
    assert!(tx_ids(dir.path()).is_empty());
    assert!(!dir.path().join(".txpt/tx").join(&previous_id).exists());
    assert!(txpt::storage::resolve_tx_id(dir.path(), None).is_err());
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
