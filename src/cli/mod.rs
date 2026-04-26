#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::diff::{self, ChangeEntry};
use crate::ignore::Policy;
use crate::manifest;
use crate::rollback;
use crate::root;
use crate::runner;
use crate::snapshot::{self, SnapshotMode};
use crate::storage;

#[derive(Debug)]
struct RunOptions {
    root: Option<PathBuf>,
    snapshot: SnapshotMode,
    json: bool,
    stream: bool,
    include_ignored: bool,
    include_sensitive: bool,
    strict: bool,
    keep: bool,
    shell_command: Option<String>,
    display_argv: Option<Vec<String>>,
    argv: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    id: String,
    version: u8,
    root: String,
    cwd: String,
    platform: String,
    started_at: String,
    finished_at: String,
    state_dir: String,
    snapshot_engine: String,
    child_exit_code: i32,
    txpt_status: String,
    rollback_guarantee: String,
    keep: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandMeta {
    argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    real_argv: Option<Vec<String>>,
    uid: u32,
    gid: u32,
    env_redacted: bool,
    pid: Option<u32>,
    shell: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunReport {
    id: String,
    root: String,
    command: Vec<String>,
    child_exit_code: i32,
    snapshot_engine: String,
    changes: BTreeMap<String, usize>,
    rollback: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShimPointMeta {
    id: String,
    root: String,
    cwd: String,
    started_at: String,
    snapshot_engine: String,
    command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionPolicy {
    protect: Vec<String>,
    ignore: Vec<String>,
}

pub fn run(args: Vec<String>) -> Result<i32> {
    if args.is_empty() {
        return no_args();
    }
    if args[0] == "--" {
        return run_command(parse_run(args[1..].to_vec())?);
    }
    match args[0].as_str() {
        "run" => run_command(parse_run(args[1..].to_vec())?),
        "diff" => show_diff(args[1..].to_vec()),
        "undo" | "rollback" => undo(args[1..].to_vec()),
        "show" => show(args[1..].to_vec()),
        "list" | "ls" => list(args[1..].to_vec()),
        "prune" => prune(),
        "shims" => shims(args[1..].to_vec()),
        "shim-exec" => shim_exec(args[1..].to_vec()),
        "shim-should-wrap" => shim_should_wrap(args[1..].to_vec()),
        "shim-begin" => shim_begin(args[1..].to_vec()),
        "shim-finish" => shim_finish(args[1..].to_vec()),
        "argv-json" => argv_json(args[1..].to_vec()),
        "--help" | "-h" => {
            print_help();
            Ok(0)
        }
        other => {
            eprintln!("txpt: unknown subcommand {other}\n");
            print_help();
            Ok(64)
        }
    }
}

fn no_args() -> Result<i32> {
    if env::var_os("TXPT_ACTIVE").is_some() {
        return session_dashboard();
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return start_session();
    }
    print_help();
    Ok(0)
}

fn parse_run(args: Vec<String>) -> Result<RunOptions> {
    let mut opts = RunOptions {
        root: None,
        snapshot: SnapshotMode::Auto,
        json: false,
        stream: true,
        include_ignored: false,
        include_sensitive: false,
        strict: false,
        keep: false,
        shell_command: None,
        display_argv: None,
        argv: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                opts.argv = args[i + 1..].to_vec();
                break;
            }
            "--root" => {
                i += 1;
                opts.root = Some(PathBuf::from(
                    args.get(i).context("--root requires a value")?,
                ));
            }
            "--snapshot" => {
                i += 1;
                opts.snapshot =
                    snapshot::parse_mode(args.get(i).context("--snapshot requires a value")?)?;
            }
            "--json" => {
                opts.json = true;
                opts.stream = false;
            }
            "--no-stream" => opts.stream = false,
            "--include-ignored" => opts.include_ignored = true,
            "--include-sensitive" => opts.include_sensitive = true,
            "--strict" => opts.strict = true,
            "--keep" => opts.keep = true,
            "--display-command" => {
                i += 1;
                opts.display_argv = Some(parse_display_command(
                    args.get(i).context("--display-command requires a value")?,
                )?);
            }
            "--shell" => {
                let command = parse_shell_command(&args, i + 1)?;
                opts.shell_command = Some(command);
                break;
            }
            other => {
                opts.argv = args[i..].to_vec();
                if other.starts_with('-') {
                    bail!("unknown run option {other}");
                }
                break;
            }
        }
        i += 1;
    }
    if opts.argv.is_empty() && opts.shell_command.is_none() {
        bail!("missing command");
    }
    Ok(opts)
}

fn run_command(opts: RunOptions) -> Result<i32> {
    let command = command_to_run(&opts)?;
    if command_uses_sudo(&command.argv, opts.shell_command.as_deref())
        && opts.snapshot != SnapshotMode::Off
    {
        eprintln!(
            "refusing sudo command under rollback mode\n\nreason:\n  txpt can only roll back protected paths inside the transaction root.\n\nuse:\n  txpt run --snapshot off -- sudo make install"
        );
        return Ok(82);
    }
    if command_starts_nested_txpt_session(&command) {
        eprintln!(
            "refusing nested txpt session\n\nreason:\n  txpt sessions cannot be started inside another txpt transaction.\n\nuse:\n  txpt\n  txpt ls\n  txpt diff\n  txpt undo"
        );
        return Ok(64);
    }
    let root = root::detect(opts.root.as_deref())?;
    let cwd = env::current_dir()?;
    let id = storage::tx_id();
    let paths = storage::tx_paths(&root, &id)?;
    let _lock = storage::acquire_lock(&paths.state_dir)?;
    let engine = snapshot::SnapshotEngine::probe(&root, &paths.tmp_dir, opts.snapshot)
        .with_context(|| "snapshot creation failed")?;
    let policy = Policy::load(&root, opts.include_ignored, opts.include_sensitive);
    let before = manifest::scan(&root, &policy)?;
    manifest::write_jsonl(&paths.tx_dir.join("before.manifest.jsonl"), &before)?;
    if opts.strict && before.iter().any(|entry| !entry.protected) {
        bail!("strict mode refuses unprotected paths");
    }
    for entry in before.iter().filter(|entry| entry.protected) {
        engine.snapshot_entry(&root, &paths.snapshot_dir, entry)?;
    }
    let started_at = timestamp();
    let output = match runner::run_child(
        &command.argv,
        &cwd,
        &paths.tx_dir.join("stdout.log"),
        &paths.tx_dir.join("stderr.log"),
        opts.stream,
    ) {
        Ok(output) => output,
        Err(err) => {
            let _ = fs::remove_dir_all(&paths.tx_dir);
            return Err(err);
        }
    };
    let after = manifest::scan(&root, &policy)?;
    manifest::write_jsonl(&paths.tx_dir.join("after.manifest.jsonl"), &after)?;
    let mut changes = diff::diff(&before, &after);
    if engine.name() == "record-only" {
        diff::mark_record_only(&mut changes);
    }
    diff::write_jsonl(&paths.tx_dir.join("changes.jsonl"), &changes)?;
    diff::write_patch(
        &root,
        &paths.snapshot_dir,
        &paths.tx_dir.join("diff.patch"),
        &changes,
    )?;
    let report = build_report(
        &id,
        &root,
        &command.display_argv,
        output.exit_code,
        engine.name(),
        &changes,
    );
    storage::write_json(
        &paths.tx_dir.join("meta.json"),
        &Meta {
            id: id.clone(),
            version: 1,
            root: root.display().to_string(),
            cwd: cwd.display().to_string(),
            platform: crate::platform::platform_name().to_owned(),
            started_at,
            finished_at: timestamp(),
            state_dir: paths.state_dir.display().to_string(),
            snapshot_engine: engine.name().to_owned(),
            child_exit_code: output.exit_code,
            txpt_status: "recorded".to_owned(),
            rollback_guarantee: rollback_guarantee(&changes),
            keep: opts.keep,
        },
    )?;
    storage::write_json(
        &paths.tx_dir.join("command.json"),
        &CommandMeta {
            argv: command.display_argv.clone(),
            real_argv: command.real_argv.clone(),
            uid: unsafe_getuid(),
            gid: unsafe_getgid(),
            env_redacted: true,
            pid: output.pid,
            shell: command.shell.clone(),
        },
    )?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let plan = rollback::plan(&root, &paths);
        print_run_receipt(&report, &plan, &changes);
    }
    Ok(output.exit_code)
}

#[derive(Debug)]
struct CommandToRun {
    argv: Vec<String>,
    display_argv: Vec<String>,
    real_argv: Option<Vec<String>>,
    shell: Option<String>,
}

fn parse_shell_command(args: &[String], start: usize) -> Result<String> {
    if start >= args.len() {
        bail!("--shell requires a command string");
    }
    if args[start] == "--" {
        let parts = &args[start + 1..];
        if parts.is_empty() {
            bail!("--shell -- requires a command");
        }
        return Ok(parts.join(" "));
    }
    Ok(args[start..].join(" "))
}

fn command_to_run(opts: &RunOptions) -> Result<CommandToRun> {
    let Some(shell_command) = &opts.shell_command else {
        return Ok(CommandToRun {
            argv: opts.argv.clone(),
            display_argv: opts
                .display_argv
                .clone()
                .unwrap_or_else(|| opts.argv.clone()),
            real_argv: opts.display_argv.as_ref().map(|_| opts.argv.clone()),
            shell: None,
        });
    };
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    if shell_command.trim().is_empty() {
        bail!("--shell command must not be empty");
    }
    Ok(CommandToRun {
        argv: vec![shell.clone(), "-ic".to_owned(), shell_command.clone()],
        display_argv: opts
            .display_argv
            .clone()
            .unwrap_or_else(|| vec![shell_command.clone()]),
        real_argv: opts
            .display_argv
            .as_ref()
            .map(|_| vec![shell.clone(), "-ic".to_owned(), shell_command.clone()]),
        shell: Some(shell),
    })
}

fn parse_display_command(value: &str) -> Result<Vec<String>> {
    let argv: Vec<String> = serde_json::from_str(value)?;
    if argv.is_empty() {
        bail!("--display-command must not be empty");
    }
    Ok(argv)
}

fn command_uses_sudo(argv: &[String], shell_command: Option<&str>) -> bool {
    if let Some(command) = shell_command {
        return command.trim_start().starts_with("sudo ");
    }
    argv.first().is_some_and(|arg| arg == "sudo")
}

fn command_starts_nested_txpt_session(command: &CommandToRun) -> bool {
    if env::var_os("TXPT_ACTIVE").is_none() {
        return false;
    }
    command.shell.is_none()
        && command.argv.first().is_some_and(|program| {
            Path::new(program)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "txpt")
        })
        && command.argv.len() == 1
}

fn show_diff(args: Vec<String>) -> Result<i32> {
    let mut id = None;
    let mut json = false;
    let mut stat = false;
    let mut name_status = false;
    for arg in &args {
        match arg.as_str() {
            "--json" => json = true,
            "--stat" => stat = true,
            "--name-status" => name_status = true,
            value if !value.starts_with('-') => id = Some(value),
            other => bail!("unknown diff option {other}"),
        }
    }
    let root = root::detect(None)?;
    let paths = storage::existing_tx_paths(&root, id)?;
    let plan = rollback::plan(&root, &paths);
    let changes = match diff::read_jsonl(&paths.tx_dir.join("changes.jsonl")) {
        Ok(changes) => changes,
        Err(err) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "plan": plan,
                        "error": err.to_string(),
                    }))?
                );
            } else {
                println!(
                    "txpt diff {}\n\nrollback:\n  state: Broken\n\nerror:\n  {}",
                    plan.tx_id, err
                );
            }
            return Ok(0);
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "plan": plan,
                "changes": changes,
            }))?
        );
    } else if stat {
        print_diff_stat(&changes);
    } else if name_status {
        for change in changes {
            println!("{}\t{}", diff::status_letter(&change.kind), change.path);
        }
    } else {
        print_diff_human(&paths, &plan, &changes)?;
    }
    Ok(0)
}

fn undo(args: Vec<String>) -> Result<i32> {
    let mut id = None;
    let mut dry_run = false;
    let mut force = false;
    let mut json = false;
    for arg in &args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--force" => force = true,
            "--json" => json = true,
            value if !value.starts_with('-') => id = Some(value),
            other => bail!("unknown undo option {other}"),
        }
    }
    let root = root::detect(None)?;
    let paths = storage::existing_tx_paths(&root, id)?;
    let plan = rollback::plan(&root, &paths);
    if dry_run {
        if json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_rollback_plan(&plan);
        }
        return Ok(0);
    }
    if plan.summary.conflicts > 0 && !force {
        if json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_rollback_plan(&plan);
            eprintln!("\nerror:\n  rollback has conflicts; nothing was changed");
        }
        return Ok(80);
    }
    match rollback::apply_plan(&root, &paths, &plan, false, force) {
        Ok(result) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                eprintln!(
                    "txpt undo\n  restored: {}\n  removed: {}\n  conflicts: {}",
                    result.restored, result.removed, result.conflicts
                );
            }
            Ok(0)
        }
        Err(err) if err.to_string().contains("rollback unsupported") => {
            eprintln!("txpt undo: rollback unsupported");
            Ok(81)
        }
        Err(err) => Err(err),
    }
}

fn show(args: Vec<String>) -> Result<i32> {
    let mut id = None;
    let mut json = false;
    for arg in &args {
        match arg.as_str() {
            "--json" => json = true,
            value if !value.starts_with('-') => id = Some(value),
            other => bail!("unknown show option {other}"),
        }
    }
    let root = root::detect(None)?;
    let paths = storage::existing_tx_paths(&root, id)?;
    let selector = id;
    let view = match tx_view(&root, &paths, selector) {
        Ok(view) => view,
        Err(err) => {
            let id = paths
                .tx_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_owned());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "id": id,
                        "state": "broken",
                        "error": err.to_string(),
                    }))?
                );
            } else {
                println!("txpt {id}\n\nrollback:\n  state: broken\n\nerror:\n  {err}");
            }
            return Ok(0);
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        print_show_card(&view);
    }
    Ok(0)
}

fn list(args: Vec<String>) -> Result<i32> {
    let ids_only = args.iter().any(|arg| arg == "--ids");
    if args.iter().any(|arg| arg != "--ids") {
        bail!("unknown list option");
    }
    let root = root::detect(None)?;
    let ids = match storage::list_tx_ids(&root) {
        Ok(ids) => ids,
        Err(_) => return Ok(0),
    };
    if ids.is_empty() {
        return Ok(0);
    }
    if ids_only {
        for id in ids {
            println!("{id}");
        }
        return Ok(0);
    }
    println!(
        "{:<8} {:<10} {:<10} {:<5} {:<12} COMMAND",
        "ID", "AGE", "STATE", "EXIT", "CHANGES"
    );
    for (index, id) in ids.iter().enumerate() {
        let selector = if index == 0 {
            "@last".to_owned()
        } else {
            format!("@{index}")
        };
        let paths = storage::existing_tx_paths(&root, Some(id))?;
        match tx_view(&root, &paths, Some(&selector)) {
            Ok(view) => {
                println!(
                    "{:<8} {:<10} {:<10} {:<5} {:<12} {}",
                    selector,
                    age(&view.meta.started_at),
                    tx_state_label(&view.plan.state),
                    view.meta.child_exit_code,
                    change_summary(&view.changes),
                    view.command.argv.join(" "),
                );
            }
            Err(_) => {
                println!(
                    "{:<8} {:<10} {:<10} {:<5} {:<12} {}",
                    selector, "?", "broken", "-", "-", id
                );
            }
        }
    }
    Ok(0)
}

fn prune() -> Result<i32> {
    let root = root::detect(None)?;
    let tx_root = storage::state_dir(&root).join("tx");
    if !tx_root.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(tx_root)? {
        let entry = entry?;
        let meta_path = entry.path().join("meta.json");
        let keep = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| value.get("keep").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);
        if !keep {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(0)
}

fn start_session() -> Result<i32> {
    if env::var_os("TXPT_ACTIVE").is_some() {
        eprintln!(
            "txpt: refusing nested session\n\nreason:\n  a txpt session is already active.\n\nuse:\n  txpt\n  txpt ls\n  txpt diff\n  txpt undo"
        );
        return Ok(64);
    }
    let root = root::detect(None)?;
    let state = storage::init_state(&root)?;
    let id = storage::tx_id();
    let session_dir = state.join("sessions").join(&id);
    let bin_dir = session_dir.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let policy = default_session_policy();
    storage::write_json(&session_dir.join("policy.json"), &policy)?;
    write_shims(&bin_dir, &policy)?;

    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    let old_path = env::var_os("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.display(), old_path.to_string_lossy());
    prepare_shell_startup(&session_dir, &shell)?;
    eprintln!("txpt session started");
    eprintln!("root: {}", root.display());
    eprintln!("mode: protected shell");
    eprintln!("shims: {}", shim_commands(&policy).join(" "));
    eprintln!(
        "note: txpt is not a sandbox; absolute paths, shell redirections, and interpreter-driven file changes are outside session shims."
    );
    let mut command = Command::new(&shell);
    configure_interactive_shell(&mut command, &session_dir, &shell);
    let status = command
        .current_dir(env::current_dir()?)
        .env("PATH", new_path)
        .env("TXPT_ACTIVE", "1")
        .env("TXPT_SESSION_ID", &id)
        .env("TXPT_SESSION_DIR", &session_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(status.code().unwrap_or(0))
}

fn session_dashboard() -> Result<i32> {
    let root = root::detect(None)?;
    let policy = active_policy().unwrap_or_else(|_| default_session_policy());
    println!("txpt session");
    println!("\nroot:\n  {}", root.display());
    println!(
        "\npolicy:\n  {} protect rules\n  {} ignore rules",
        policy.protect.len(),
        policy.ignore.len()
    );
    if let Ok(session_dir) = active_session_dir() {
        let bin_dir = session_dir.join("bin");
        if !path_starts_with(&bin_dir) {
            println!(
                "\nwarning:\n  txpt shim directory is not first in PATH.\n  run `hash -r` or restart the txpt session."
            );
        }
    }
    println!("\nshims:");
    for command in shim_commands(&policy) {
        println!("  {command}");
    }
    println!(
        "\nnot sandboxed:\n  absolute-path commands, `command <name>`, shell redirections, pipelines, and interpreter-driven file changes"
    );
    if let Ok(paths) = storage::existing_tx_paths(&root, None) {
        if let Ok(view) = tx_view(&root, &paths, Some("@last")) {
            println!(
                "\nlast point:\n  @last  {}  {}",
                tx_state_label(&view.plan.state),
                view.command.argv.join(" ")
            );
        }
    }
    println!("\ncommands:\n  txpt diff\n  txpt undo\n  txpt rollback\n  txpt shims ls");
    Ok(0)
}

fn prepare_shell_startup(session_dir: &Path, shell: &str) -> Result<()> {
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let exe = shell_quote(&env::current_exe()?.display().to_string());
    let zsh_repath = "\
__txpt_exe='__TXPT_EXE__'

__txpt_shell_wrap() {
  emulate -L zsh
  local txpt_cmd=\"$1\"
  shift
  local txpt_orig=\"__txpt_original_$txpt_cmd\"
  if \"$__txpt_exe\" shim-should-wrap \"$txpt_cmd\" \"$@\"; then
    local txpt_bin=\"$TXPT_SESSION_DIR/bin\"
    local txpt_display txpt_id txpt_status txpt_finish txpt_old_path
    txpt_display=$(\"$__txpt_exe\" argv-json \"$txpt_cmd\" \"$@\") || return $?
    txpt_id=$(\"$__txpt_exe\" shim-begin \"$txpt_display\") || return $?
    txpt_old_path=\"$PATH\"
    path=(\"${(@)path:#$txpt_bin}\")
    export PATH
    \"$txpt_orig\" \"$@\"
    txpt_status=$?
    PATH=\"$txpt_old_path\"
    export PATH
    \"$__txpt_exe\" shim-finish \"$txpt_id\" \"$txpt_status\"
    txpt_finish=$?
    (( txpt_finish == 0 )) || return $txpt_finish
    return $txpt_status
  else
    \"$txpt_orig\" \"$@\"
  fi
}

__txpt_capture_function_shim() {
  emulate -L zsh
  local txpt_cmd=\"$1\"
  local txpt_orig=\"__txpt_original_$txpt_cmd\"
  if (( $+functions[$txpt_cmd] )) && [[ \"$functions[$txpt_cmd]\" != *__txpt_shell_wrap* ]]; then
    functions[$txpt_orig]=$functions[$txpt_cmd]
    eval \"$txpt_cmd() { __txpt_shell_wrap ${(q)txpt_cmd} \\\"\\$@\\\"; }\"
  fi
}

__txpt_repath() {
  emulate -L zsh
  local txpt_bin=\"$TXPT_SESSION_DIR/bin\"
  local txpt_cmd
  path=(\"${(@)path:#$txpt_bin}\")
  path=(\"$txpt_bin\" \"${path[@]}\")
  export PATH
  for txpt_cmd in \"$txpt_bin\"/*(N:t); do
    __txpt_capture_function_shim \"$txpt_cmd\"
    unalias \"$txpt_cmd\" 2>/dev/null || true
  done
  hash -r 2>/dev/null || true
}
__txpt_repath
autoload -Uz add-zsh-hook 2>/dev/null || true
add-zsh-hook precmd __txpt_repath 2>/dev/null || true
add-zsh-hook preexec __txpt_repath 2>/dev/null || true
";
    let bash_repath = "\
__txpt_exe='__TXPT_EXE__'

__txpt_shell_wrap() {
  local txpt_cmd=\"$1\"
  shift
  local txpt_orig=\"__txpt_original_$txpt_cmd\"
  if \"$__txpt_exe\" shim-should-wrap \"$txpt_cmd\" \"$@\"; then
    local txpt_bin=\"$TXPT_SESSION_DIR/bin\"
    local txpt_display txpt_id txpt_status txpt_finish txpt_old_path
    txpt_display=$(\"$__txpt_exe\" argv-json \"$txpt_cmd\" \"$@\") || return $?
    txpt_id=$(\"$__txpt_exe\" shim-begin \"$txpt_display\") || return $?
    txpt_old_path=\"$PATH\"
    PATH=\":$PATH:\"
    PATH=${PATH//:$txpt_bin:/:}
    PATH=${PATH#:}
    PATH=${PATH%:}
    export PATH
    \"$txpt_orig\" \"$@\"
    txpt_status=$?
    PATH=\"$txpt_old_path\"
    export PATH
    \"$__txpt_exe\" shim-finish \"$txpt_id\" \"$txpt_status\"
    txpt_finish=$?
    [ \"$txpt_finish\" -eq 0 ] || return \"$txpt_finish\"
    return \"$txpt_status\"
  else
    \"$txpt_orig\" \"$@\"
  fi
}

__txpt_capture_function_shim() {
  local txpt_cmd=\"$1\"
  local txpt_orig=\"__txpt_original_$txpt_cmd\"
  if declare -F \"$txpt_cmd\" >/dev/null && ! declare -f \"$txpt_cmd\" | grep -q __txpt_shell_wrap; then
    eval \"$(declare -f \"$txpt_cmd\" | sed \"1s/^$txpt_cmd[[:space:]]*/$txpt_orig /\")\"
    eval \"$txpt_cmd() { __txpt_shell_wrap '$txpt_cmd' \\\"\\$@\\\"; }\"
  fi
}

__txpt_repath() {
  local txpt_bin=\"$TXPT_SESSION_DIR/bin\"
  local txpt_file txpt_cmd
  local old_path=\":$PATH:\"
  old_path=${old_path//:$txpt_bin:/:}
  old_path=${old_path#:}
  old_path=${old_path%:}
  export PATH=\"$txpt_bin:$old_path\"
  for txpt_file in \"$txpt_bin\"/*; do
    [ -e \"$txpt_file\" ] || continue
    txpt_cmd=${txpt_file##*/}
    __txpt_capture_function_shim \"$txpt_cmd\"
    unalias \"$txpt_cmd\" 2>/dev/null || true
  done
  hash -r 2>/dev/null || true
}
__txpt_repath
case \";${PROMPT_COMMAND:-};\" in
  *\";__txpt_repath;\"*) ;;
  *) PROMPT_COMMAND=\"__txpt_repath${PROMPT_COMMAND:+;$PROMPT_COMMAND}\" ;;
esac
";
    match shell_name {
        "zsh" => {
            fs::write(
                session_dir.join(".zshenv"),
                "[ -f \"$HOME/.zshenv\" ] && . \"$HOME/.zshenv\"\n",
            )?;
            fs::write(
                session_dir.join(".zshrc"),
                format!(
                    "[ -f \"$HOME/.zshrc\" ] && . \"$HOME/.zshrc\"\n{}export PS1=\"(txpt) $PS1\"\n",
                    zsh_repath.replace("__TXPT_EXE__", &exe)
                ),
            )?;
        }
        "bash" => {
            fs::write(
                session_dir.join(".bashrc"),
                format!(
                    "[ -f \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n{}export PS1=\"(txpt) $PS1\"\n",
                    bash_repath.replace("__TXPT_EXE__", &exe)
                ),
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn configure_interactive_shell(command: &mut Command, session_dir: &Path, shell: &str) {
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    match shell_name {
        "zsh" => {
            command.arg("-i").env("ZDOTDIR", session_dir);
        }
        "bash" => {
            command
                .arg("--rcfile")
                .arg(session_dir.join(".bashrc"))
                .arg("-i");
        }
        _ => {
            command.arg("-i");
        }
    };
}

fn path_starts_with(bin_dir: &Path) -> bool {
    env::var_os("PATH")
        .and_then(|path| env::split_paths(&path).next())
        .is_some_and(|first| first == bin_dir)
}

fn shims(args: Vec<String>) -> Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        return shims_status();
    };
    match command {
        "list" | "ls" | "status" => shims_status(),
        "protect" => update_protect_rules(&args[1..], true),
        "unprotect" => update_protect_rules(&args[1..], false),
        "ignore" => update_ignore_rules(&args[1..], true),
        "unignore" => update_ignore_rules(&args[1..], false),
        "edit" => edit_shim_policy(),
        other => bail!("unknown shims command {other}"),
    }
}

fn shims_status() -> Result<i32> {
    let policy = active_policy()?;
    println!("shims:");
    for command in shim_commands(&policy) {
        println!("  {command}");
    }
    println!("\nprotect:");
    for rule in &policy.protect {
        println!("  {rule}");
    }
    println!("\nignore:");
    for rule in &policy.ignore {
        println!("  {rule}");
    }
    Ok(0)
}

fn update_protect_rules(rules: &[String], add: bool) -> Result<i32> {
    if rules.is_empty() {
        bail!(
            "shims {} requires at least one command pattern",
            if add { "protect" } else { "unprotect" }
        );
    }
    let session_dir = active_session_dir()?;
    let mut policy = active_policy()?;
    for rule in rules {
        let pattern = bash_rule_pattern(rule);
        if add {
            add_unique(&mut policy.protect, pattern);
        } else {
            policy.protect.retain(|existing| existing != &pattern);
        }
    }
    storage::write_json(&session_dir.join("policy.json"), &policy)?;
    write_shims(&session_dir.join("bin"), &policy)?;
    Ok(0)
}

fn update_ignore_rules(rules: &[String], add: bool) -> Result<i32> {
    if rules.is_empty() {
        bail!(
            "shims {} requires at least one command pattern",
            if add { "ignore" } else { "unignore" }
        );
    }
    let session_dir = active_session_dir()?;
    let mut policy = active_policy()?;
    for rule in rules {
        let pattern = bash_rule_pattern(rule);
        if add {
            add_unique(&mut policy.ignore, pattern);
        } else {
            policy.ignore.retain(|existing| existing != &pattern);
        }
    }
    storage::write_json(&session_dir.join("policy.json"), &policy)?;
    write_shims(&session_dir.join("bin"), &policy)?;
    Ok(0)
}

fn edit_shim_policy() -> Result<i32> {
    let session_dir = active_session_dir()?;
    let path = session_dir.join("policy.json");
    let editor = env::var_os("EDITOR")
        .filter(|value| !value.is_empty())
        .context("EDITOR is not set")?;
    let status = Command::new(editor)
        .arg(&path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to launch EDITOR")?;
    if !status.success() {
        bail!("editor exited with {}", status.code().unwrap_or(1));
    }
    let policy = active_policy().context("edited policy.json is invalid")?;
    write_shims(&session_dir.join("bin"), &policy)?;
    Ok(0)
}

fn shim_exec(args: Vec<String>) -> Result<i32> {
    let Some(command) = args.first() else {
        bail!("shim-exec requires a command");
    };
    let rest = args[1..].to_vec();
    let policy = active_policy()?;
    let real = find_real_command(command)?;
    let mut real_argv = vec![real.display().to_string()];
    real_argv.extend(rest.clone());
    if should_wrap(&policy, command, &rest) {
        let mut display_argv = vec![command.clone()];
        display_argv.extend(rest);
        return run_command(RunOptions {
            root: None,
            snapshot: SnapshotMode::Auto,
            json: false,
            stream: true,
            include_ignored: true,
            include_sensitive: false,
            strict: false,
            keep: false,
            shell_command: None,
            display_argv: Some(display_argv),
            argv: real_argv,
        });
    }
    let status = Command::new(&real)
        .args(rest)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(status.code().unwrap_or(1))
}

fn shim_should_wrap(args: Vec<String>) -> Result<i32> {
    let Some(command) = args.first() else {
        bail!("shim-should-wrap requires a command");
    };
    let policy = active_policy()?;
    Ok(if should_wrap(&policy, command, &args[1..]) {
        0
    } else {
        1
    })
}

fn shim_begin(args: Vec<String>) -> Result<i32> {
    let Some(display_json) = args.first() else {
        bail!("shim-begin requires display argv JSON");
    };
    let display_argv = parse_display_command(display_json)?;
    let root = root::detect(None)?;
    let cwd = env::current_dir()?;
    let id = storage::tx_id();
    let paths = storage::tx_paths(&root, &id)?;
    let _lock = storage::acquire_lock(&paths.state_dir)?;
    let engine = snapshot::SnapshotEngine::probe(&root, &paths.tmp_dir, SnapshotMode::Auto)
        .with_context(|| "snapshot creation failed")?;
    let policy = Policy::load(&root, true, false);
    let before = manifest::scan(&root, &policy)?;
    manifest::write_jsonl(&paths.tx_dir.join("before.manifest.jsonl"), &before)?;
    for entry in before.iter().filter(|entry| entry.protected) {
        engine.snapshot_entry(&root, &paths.snapshot_dir, entry)?;
    }
    let started_at = timestamp();
    storage::write_json(
        &paths.tx_dir.join("shim-point.json"),
        &ShimPointMeta {
            id: id.clone(),
            root: root.display().to_string(),
            cwd: cwd.display().to_string(),
            started_at,
            snapshot_engine: engine.name().to_owned(),
            command: display_argv.clone(),
        },
    )?;
    storage::write_json(
        &paths.tx_dir.join("command.json"),
        &CommandMeta {
            argv: display_argv,
            real_argv: None,
            uid: unsafe_getuid(),
            gid: unsafe_getgid(),
            env_redacted: true,
            pid: None,
            shell: Some("txpt-session-function".to_owned()),
        },
    )?;
    fs::File::create(paths.tx_dir.join("stdout.log"))?;
    fs::File::create(paths.tx_dir.join("stderr.log"))?;
    println!("{id}");
    Ok(0)
}

fn shim_finish(args: Vec<String>) -> Result<i32> {
    let Some(id) = args.first() else {
        bail!("shim-finish requires transaction id");
    };
    let exit_code = args
        .get(1)
        .context("shim-finish requires child exit code")?
        .parse::<i32>()
        .context("invalid child exit code")?;
    let root = root::detect(None)?;
    let paths = storage::existing_tx_paths(&root, Some(id))?;
    let point: ShimPointMeta = storage::read_json(&paths.tx_dir.join("shim-point.json"))?;
    let policy = Policy::load(&root, true, false);
    let before = manifest::read_jsonl(&paths.tx_dir.join("before.manifest.jsonl"))?;
    let after = manifest::scan(&root, &policy)?;
    manifest::write_jsonl(&paths.tx_dir.join("after.manifest.jsonl"), &after)?;
    let mut changes = diff::diff(&before, &after);
    if point.snapshot_engine == "record-only" {
        diff::mark_record_only(&mut changes);
    }
    diff::write_jsonl(&paths.tx_dir.join("changes.jsonl"), &changes)?;
    diff::write_patch(
        &root,
        &paths.snapshot_dir,
        &paths.tx_dir.join("diff.patch"),
        &changes,
    )?;
    storage::write_json(
        &paths.tx_dir.join("meta.json"),
        &Meta {
            id: point.id.clone(),
            version: 1,
            root: point.root.clone(),
            cwd: point.cwd.clone(),
            platform: crate::platform::platform_name().to_owned(),
            started_at: point.started_at,
            finished_at: timestamp(),
            state_dir: paths.state_dir.display().to_string(),
            snapshot_engine: point.snapshot_engine.clone(),
            child_exit_code: exit_code,
            txpt_status: "recorded".to_owned(),
            rollback_guarantee: rollback_guarantee(&changes),
            keep: false,
        },
    )?;
    let report = build_report(
        &point.id,
        &root,
        &point.command,
        exit_code,
        &point.snapshot_engine,
        &changes,
    );
    let plan = rollback::plan(&root, &paths);
    print_run_receipt(&report, &plan, &changes);
    Ok(0)
}

fn argv_json(args: Vec<String>) -> Result<i32> {
    if args.is_empty() {
        bail!("argv-json requires at least one argument");
    }
    println!("{}", serde_json::to_string(&args)?);
    Ok(0)
}

fn default_session_policy() -> SessionPolicy {
    SessionPolicy {
        protect: vec![
            "rm:*".to_owned(),
            "unlink:*".to_owned(),
            "rmdir:*".to_owned(),
            "mv:*".to_owned(),
            "cp:*".to_owned(),
            "ln:*".to_owned(),
            "mkdir:*".to_owned(),
            "touch:*".to_owned(),
            "chmod:*".to_owned(),
            "chown:*".to_owned(),
            "truncate:*".to_owned(),
            "patch:*".to_owned(),
            "tee:*".to_owned(),
            "rsync:*".to_owned(),
            "dd:*".to_owned(),
            "sed:* -i:*".to_owned(),
            "sed:* --in-place:*".to_owned(),
            "perl:* -i:*".to_owned(),
            "find:* -delete:*".to_owned(),
            "find:* -exec rm:*".to_owned(),
            "find:* -execdir rm:*".to_owned(),
            "xargs:* rm:*".to_owned(),
            "git clean:*".to_owned(),
            "git reset --hard:*".to_owned(),
            "git restore:*".to_owned(),
            "git checkout --:*".to_owned(),
            "git rm:*".to_owned(),
            "git apply:*".to_owned(),
            "git stash pop:*".to_owned(),
            "git stash apply:*".to_owned(),
            "npm install:*".to_owned(),
            "npm update:*".to_owned(),
            "npm uninstall:*".to_owned(),
            "npm audit fix:*".to_owned(),
            "npm add:*".to_owned(),
            "npm remove:*".to_owned(),
            "pnpm install:*".to_owned(),
            "pnpm add:*".to_owned(),
            "pnpm update:*".to_owned(),
            "pnpm remove:*".to_owned(),
            "yarn install:*".to_owned(),
            "yarn add:*".to_owned(),
            "yarn remove:*".to_owned(),
            "yarn upgrade:*".to_owned(),
            "bun install:*".to_owned(),
            "bun add:*".to_owned(),
            "bun remove:*".to_owned(),
            "bun update:*".to_owned(),
            "cargo update:*".to_owned(),
            "cargo add:*".to_owned(),
            "cargo remove:*".to_owned(),
            "go get:*".to_owned(),
            "go mod tidy:*".to_owned(),
            "poetry install:*".to_owned(),
            "poetry add:*".to_owned(),
            "poetry remove:*".to_owned(),
            "poetry update:*".to_owned(),
            "uv add:*".to_owned(),
            "uv remove:*".to_owned(),
            "uv sync:*".to_owned(),
            "uv lock:*".to_owned(),
        ],
        ignore: vec![
            "* --help".to_owned(),
            "* -h".to_owned(),
            "* --version".to_owned(),
            "* -v".to_owned(),
            "* version".to_owned(),
            "* help".to_owned(),
        ],
    }
}

fn write_shims(bin_dir: &Path, policy: &SessionPolicy) -> Result<()> {
    fs::create_dir_all(bin_dir)?;
    for entry in fs::read_dir(bin_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    let exe = env::current_exe()?;
    for command in shim_commands(policy) {
        let path = bin_dir.join(&command);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nexec '{}' shim-exec '{}' \"$@\"\n",
                shell_quote(&exe.display().to_string()),
                shell_quote(&command)
            ),
        )?;
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn active_session_dir() -> Result<PathBuf> {
    env::var_os("TXPT_SESSION_DIR")
        .map(PathBuf::from)
        .context("not inside a txpt session")
}

fn active_policy() -> Result<SessionPolicy> {
    storage::read_json(&active_session_dir()?.join("policy.json"))
}

fn should_wrap(policy: &SessionPolicy, command: &str, args: &[String]) -> bool {
    let command_line = shell_display_command(command, args);
    policy
        .protect
        .iter()
        .map(|rule| bash_rule_pattern(rule))
        .any(|pattern| bash_pattern_matches(&pattern, &command_line))
        && !policy
            .ignore
            .iter()
            .map(|rule| bash_rule_pattern(rule))
            .any(|pattern| bash_pattern_matches(&pattern, &command_line))
}

fn shell_display_command(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn bash_rule_pattern(rule: &str) -> String {
    let trimmed = rule.trim();
    trimmed
        .strip_prefix("Bash(")
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(trimmed)
        .trim()
        .to_owned()
}

fn bash_pattern_matches(pattern: &str, command: &str) -> bool {
    let pattern = normalize_bash_pattern(pattern);
    if pattern == "*" {
        return true;
    }
    wildcard_match(&pattern, command)
}

fn normalize_bash_pattern(pattern: &str) -> String {
    if !pattern.contains(":*") {
        return pattern.to_owned();
    }
    if let Some(prefix) = pattern.strip_suffix(":*") {
        let prefix = prefix.replace(":*", "*");
        if !prefix.contains('*') {
            return format!("{prefix} *");
        }
        return format!("{prefix}*");
    }
    pattern.replace(":*", "*")
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if let Some(prefix) = pattern
        .strip_suffix(" *")
        .filter(|prefix| !prefix.contains('*'))
    {
        return value == prefix || value.starts_with(&format!("{prefix} "));
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut rest = value;
    if let Some(first) = parts.first().filter(|first| !first.is_empty()) {
        let Some(next) = rest.strip_prefix(first) else {
            return false;
        };
        rest = next;
    }
    for (index, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            continue;
        }
        let Some(pos) = rest.find(part) else {
            return false;
        };
        if index == parts.len() - 1 && !pattern.ends_with('*') {
            let after = &rest[pos + part.len()..];
            return after.is_empty();
        }
        rest = &rest[pos + part.len()..];
    }
    pattern.ends_with('*') || rest.is_empty()
}

fn shim_command_from_pattern(pattern: &str) -> Option<String> {
    let first = pattern
        .split_whitespace()
        .next()
        .map(|part| part.trim_end_matches(":*"))
        .filter(|part| !part.contains('*'))?;
    Some(first.to_owned())
}

fn shim_commands(policy: &SessionPolicy) -> Vec<String> {
    let mut commands = Vec::new();
    for pattern in policy.protect.iter().chain(policy.ignore.iter()) {
        if let Some(command) = shim_command_from_pattern(pattern) {
            add_unique(&mut commands, command);
        }
    }
    commands.retain(|command| should_create_shim(command));
    commands
}

fn should_create_shim(command: &str) -> bool {
    !matches!(
        command,
        "" | "."
            | ".."
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "sudo"
            | "su"
            | "doas"
            | "env"
            | "exec"
            | "command"
            | "xargs"
            | "nohup"
            | "open"
            | "txpt"
    )
}

fn add_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
        values.sort();
    }
}

fn find_real_command(command: &str) -> Result<PathBuf> {
    if command.contains('/') {
        return Ok(PathBuf::from(command));
    }
    let session_bin = env::var_os("TXPT_SESSION_DIR").map(|dir| PathBuf::from(dir).join("bin"));
    for dir in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        if session_bin
            .as_ref()
            .is_some_and(|session_bin| &dir == session_bin)
        {
            continue;
        }
        let candidate = dir.join(command);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("failed to find real command {command}")
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn build_report(
    id: &str,
    root: &Path,
    command: &[String],
    exit_code: i32,
    engine: &str,
    changes: &[ChangeEntry],
) -> RunReport {
    let mut change_counts = BTreeMap::from([
        ("modified".to_owned(), 0),
        ("created".to_owned(), 0),
        ("deleted".to_owned(), 0),
        ("unprotected".to_owned(), 0),
    ]);
    let mut rollback_counts = BTreeMap::from([
        ("full".to_owned(), 0),
        ("cleanup_only".to_owned(), 0),
        ("conflict".to_owned(), 0),
        ("unprotected".to_owned(), 0),
    ]);
    for change in changes {
        match change.kind {
            diff::ChangeKind::CreatedFile | diff::ChangeKind::CreatedDir => {
                increment(&mut change_counts, "created")
            }
            diff::ChangeKind::DeletedFile => increment(&mut change_counts, "deleted"),
            diff::ChangeKind::Unprotected => increment(&mut change_counts, "unprotected"),
            _ => increment(&mut change_counts, "modified"),
        }
        *rollback_counts.entry(change.guarantee.clone()).or_default() += 1;
    }
    RunReport {
        id: id.to_owned(),
        root: root.display().to_string(),
        command: command.to_vec(),
        child_exit_code: exit_code,
        snapshot_engine: engine.to_owned(),
        changes: change_counts,
        rollback: rollback_counts,
    }
}

#[derive(Debug, Serialize)]
struct TxView {
    id: String,
    selector: String,
    meta: Meta,
    command: CommandMeta,
    changes: Vec<ChangeEntry>,
    plan: rollback::RollbackPlan,
}

fn tx_view(root: &Path, paths: &storage::TxPaths, selector: Option<&str>) -> Result<TxView> {
    let meta: Meta = storage::read_json(&paths.tx_dir.join("meta.json"))?;
    let command: CommandMeta = storage::read_json(&paths.tx_dir.join("command.json"))?;
    let changes = diff::read_jsonl(&paths.tx_dir.join("changes.jsonl"))?;
    let plan = rollback::plan(root, paths);
    Ok(TxView {
        id: meta.id.clone(),
        selector: selector.unwrap_or("@last").to_owned(),
        meta,
        command,
        changes,
        plan,
    })
}

fn print_show_card(view: &TxView) {
    println!("txpt {}  {}", view.selector, view.id);
    println!("\ncommand:\n  {}", view.command.argv.join(" "));
    println!("\ntime:\n  started: {}", view.meta.started_at);
    println!("\nroot:\n  {}", view.meta.root);
    println!("\nsnapshot:\n  engine: {}", view.meta.snapshot_engine);
    println!("\nchanges:");
    for (label, count) in grouped_change_counts(&view.changes) {
        println!("  {label:<11} {count}");
    }
    println!(
        "\nrollback:\n  state: {}\n  restorable: {} paths\n  removable: {} paths\n  conflicts: {} paths\n  unprotected: {} paths",
        tx_state_label(&view.plan.state),
        view.plan.summary.restorable,
        view.plan.summary.removable,
        view.plan.summary.conflicts,
        view.plan.summary.unprotected,
    );
    if view.plan.summary.conflicts > 0 {
        println!("\nconflicts:");
        for entry in view
            .plan
            .entries
            .iter()
            .filter(|entry| entry.status == rollback::RollbackStatus::Conflict)
        {
            println!(
                "  {}\n    reason: {}",
                entry.path,
                entry.reason.as_deref().unwrap_or("current state changed")
            );
        }
    }
    println!(
        "\ncommands:\n  txpt diff {}\n  txpt undo {} --dry-run\n  txpt rollback {} --dry-run",
        view.id, view.id, view.id
    );
}

fn print_diff_human(
    paths: &storage::TxPaths,
    plan: &rollback::RollbackPlan,
    changes: &[ChangeEntry],
) -> Result<()> {
    println!("txpt diff {}", plan.tx_id);
    println!(
        "\nrollback:\n  state: {}\n  can undo: {} paths\n  conflicts: {} paths\n  unprotected: {} paths",
        tx_state_label(&plan.state),
        plan.summary.restorable + plan.summary.removable,
        plan.summary.conflicts,
        plan.summary.unprotected,
    );
    for change in changes {
        let status = plan
            .entries
            .iter()
            .find(|entry| entry.path == change.path)
            .map(|entry| {
                (
                    format!("{:?}", entry.status).to_lowercase(),
                    entry.reason.as_deref().unwrap_or("").to_owned(),
                )
            })
            .unwrap_or_else(|| (change.guarantee.clone(), String::new()));
        println!(
            "\n{} {}\n  rollback: {}",
            diff::status_letter(&change.kind),
            change.path,
            status.0
        );
        if !status.1.is_empty() {
            println!("  reason: {}", status.1);
        }
    }
    let patch = paths.tx_dir.join("diff.patch");
    if patch.exists() {
        println!("\n{}", fs::read_to_string(patch)?);
    }
    Ok(())
}

fn print_diff_stat(changes: &[ChangeEntry]) {
    for change in changes {
        println!(
            "{:<40} before={:<8?} after={:<8?}",
            change.path, change.before_size, change.after_size
        );
    }
}

fn grouped_change_counts(changes: &[ChangeEntry]) -> Vec<(&'static str, usize)> {
    let mut modified = 0;
    let mut created = 0;
    let mut deleted = 0;
    let mut unprotected = 0;
    for change in changes {
        match change.kind {
            diff::ChangeKind::CreatedFile | diff::ChangeKind::CreatedDir => created += 1,
            diff::ChangeKind::DeletedFile => deleted += 1,
            diff::ChangeKind::Unprotected => unprotected += 1,
            _ => modified += 1,
        }
    }
    vec![
        ("modified", modified),
        ("created", created),
        ("deleted", deleted),
        ("unprotected", unprotected),
    ]
}

fn change_summary(changes: &[ChangeEntry]) -> String {
    let counts = grouped_change_counts(changes);
    let get = |name: &str| {
        counts
            .iter()
            .find(|(label, _)| *label == name)
            .map(|(_, count)| *count)
            .unwrap_or(0)
    };
    format!(
        "~{} +{} -{} !{}",
        get("modified"),
        get("created"),
        get("deleted"),
        get("unprotected")
    )
}

fn age(started_at: &str) -> String {
    let Some(rest) = started_at.strip_prefix("unix:") else {
        return "?".to_owned();
    };
    let Ok(start) = rest.parse::<u64>() else {
        return "?".to_owned();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed = now.saturating_sub(start);
    if elapsed < 60 {
        format!("{elapsed}s")
    } else if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else {
        format!("{}h", elapsed / 3600)
    }
}

fn increment(counts: &mut BTreeMap<String, usize>, key: &str) {
    *counts.entry(key.to_owned()).or_default() += 1;
}

fn print_run_receipt(report: &RunReport, plan: &rollback::RollbackPlan, changes: &[ChangeEntry]) {
    eprintln!(
        "\ntxpt point @last  {}\n\ncommand:\n  {}\n\nresult:\n  exit {}\n\nchanged:",
        report.id,
        report.command.join(" "),
        report.child_exit_code,
    );
    for change in changes.iter().take(12) {
        eprintln!("  {} {}", diff::status_letter(&change.kind), change.path);
    }
    if changes.len() > 12 {
        eprintln!("  ... {} more", changes.len() - 12);
    }
    if report
        .command
        .first()
        .is_some_and(|command| command == "git")
    {
        eprintln!(
            "\nwarning:\n  git command may modify .git state.\n  txpt restores protected workspace files, not Git index or repository metadata."
        );
    }
    eprintln!(
        "\nrollback:\n  state: {}\n  will restore: {} paths\n  will remove:  {} paths\n\nnext:\n  inspect: txpt diff @last\n  preview: txpt undo @last --dry-run\n  undo:    txpt undo @last\n  alias:   txpt rollback @last",
        tx_state_label(&plan.state),
        plan.summary.restorable,
        plan.summary.removable,
    );
}

fn print_rollback_plan(plan: &rollback::RollbackPlan) {
    eprintln!(
        "txpt undo plan {}\n\nrollback:\n  state: {:?}\n  restorable: {}\n  removable: {}\n  conflicts: {}\n  unprotected: {}",
        plan.tx_id,
        plan.state,
        plan.summary.restorable,
        plan.summary.removable,
        plan.summary.conflicts,
        plan.summary.unprotected,
    );
    if plan.summary.conflicts > 0 {
        eprintln!("\nconflicts:");
        for entry in plan
            .entries
            .iter()
            .filter(|entry| entry.status == rollback::RollbackStatus::Conflict)
        {
            eprintln!(
                "  {}\n    reason: {}",
                entry.path,
                entry
                    .reason
                    .as_deref()
                    .unwrap_or("current state does not match recorded after state")
            );
        }
    }
}

fn rollback_guarantee(changes: &[ChangeEntry]) -> String {
    if changes
        .iter()
        .all(|change| change.guarantee == "full" || change.guarantee == "cleanup_only")
    {
        "full".to_owned()
    } else {
        "partial".to_owned()
    }
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn print_help() {
    eprintln!(
        "txpt creates reversible transaction points around Unix commands\n\nusage:\n  txpt                         # protected shell session when interactive\n  txpt -- <cmd> [args...]\n  txpt run [options] -- <cmd> [args...]\n  txpt run [options] --shell '<shell command>'\n  txpt diff [TX_ID]\n  txpt undo|rollback [TX_ID] [--dry-run] [--force] [--json]\n  txpt list|ls\n  txpt show [TX_ID]\n  txpt shims list|ls|status\n  txpt shims protect <command-pattern>...\n  txpt shims unprotect <command-pattern>...\n  txpt shims ignore <command-pattern>...\n  txpt shims unignore <command-pattern>...\n  txpt shims edit\n  txpt prune"
    );
}

fn tx_state_label(state: &rollback::TxState) -> &'static str {
    match state {
        rollback::TxState::Undoable => "undoable",
        rollback::TxState::Partial => "partial",
        rollback::TxState::Conflict => "conflict",
        rollback::TxState::Reverted => "reverted",
        rollback::TxState::RecordOnly => "record-only",
        rollback::TxState::Broken => "broken",
    }
}

fn unsafe_getuid() -> u32 {
    // SAFETY: getuid has no preconditions and returns the process uid.
    unsafe { libc::getuid() }
}

fn unsafe_getgid() -> u32 {
    // SAFETY: getgid has no preconditions and returns the process gid.
    unsafe { libc::getgid() }
}
