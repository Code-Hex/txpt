#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

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
    argv: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Meta {
    id: String,
    version: u8,
    root: String,
    cwd: String,
    platform: &'static str,
    started_at: String,
    finished_at: String,
    state_dir: String,
    snapshot_engine: String,
    child_exit_code: i32,
    txpt_status: String,
    rollback_guarantee: String,
    keep: bool,
}

#[derive(Debug, Serialize)]
struct CommandMeta {
    argv: Vec<String>,
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

pub fn run(args: Vec<String>) -> Result<i32> {
    if args.is_empty() {
        print_help();
        return Ok(0);
    }
    if args[0] == "--" {
        return run_command(parse_run(args[1..].to_vec())?);
    }
    match args[0].as_str() {
        "run" => run_command(parse_run(args[1..].to_vec())?),
        "diff" => show_diff(args.get(1).map(String::as_str)),
        "undo" => undo(args[1..].to_vec()),
        "show" => show(args.get(1).map(String::as_str)),
        "list" => list(),
        "prune" => prune(),
        "doctor" => doctor(),
        "inspect" => inspect(&args[1..]),
        "--help" | "-h" => {
            print_help();
            Ok(0)
        }
        _ => {
            let mut run_args = args;
            if run_args.first().is_some_and(|arg| arg == "--") {
                run_args.remove(0);
            }
            run_command(parse_run(run_args)?)
        }
    }
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
            "refusing sudo command under rollback mode\n\nreason:\n  txpt can only roll back protected paths inside the transaction root.\n\nuse:\n  txpt --snapshot off -- sudo make install"
        );
        return Ok(82);
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
    let output = runner::run_child(
        &command.argv,
        &cwd,
        &paths.tx_dir.join("stdout.log"),
        &paths.tx_dir.join("stderr.log"),
        opts.stream,
    )?;
    let after = manifest::scan(&root, &policy)?;
    manifest::write_jsonl(&paths.tx_dir.join("after.manifest.jsonl"), &after)?;
    let changes = diff::diff(&before, &after);
    diff::write_jsonl(&paths.tx_dir.join("changes.jsonl"), &changes)?;
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
            platform: crate::platform::platform_name(),
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
        print_human_report(&report);
    }
    Ok(output.exit_code)
}

#[derive(Debug)]
struct CommandToRun {
    argv: Vec<String>,
    display_argv: Vec<String>,
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
            display_argv: opts.argv.clone(),
            shell: None,
        });
    };
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    if shell_command.trim().is_empty() {
        bail!("--shell command must not be empty");
    }
    Ok(CommandToRun {
        argv: vec![shell.clone(), "-ic".to_owned(), shell_command.clone()],
        display_argv: vec![shell_command.clone()],
        shell: Some(shell),
    })
}

fn command_uses_sudo(argv: &[String], shell_command: Option<&str>) -> bool {
    if let Some(command) = shell_command {
        return command.trim_start().starts_with("sudo ");
    }
    argv.first().is_some_and(|arg| arg == "sudo")
}

fn show_diff(id: Option<&str>) -> Result<i32> {
    let root = root::detect(None)?;
    let paths = storage::existing_tx_paths(&root, id)?;
    let changes = diff::read_jsonl(&paths.tx_dir.join("changes.jsonl"))?;
    for change in changes {
        println!(
            "{:<16} {}",
            format!("{:?}", change.kind).to_lowercase(),
            change.path
        );
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
    match rollback::undo(&root, &paths, dry_run, force) {
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
        Err(err) if err.to_string().contains("rollback conflict") => {
            eprintln!("txpt undo: rollback conflict");
            Ok(80)
        }
        Err(err) => Err(err),
    }
}

fn show(id: Option<&str>) -> Result<i32> {
    let root = root::detect(None)?;
    let paths = storage::existing_tx_paths(&root, id)?;
    println!(
        "{}",
        std::fs::read_to_string(paths.tx_dir.join("meta.json"))?
    );
    Ok(0)
}

fn list() -> Result<i32> {
    let root = root::detect(None)?;
    let tx_root = storage::state_dir(&root).join("tx");
    if !tx_root.exists() {
        return Ok(0);
    }
    let mut entries = std::fs::read_dir(tx_root)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if entry.file_type()?.is_dir() {
            println!("{}", entry.file_name().to_string_lossy());
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

fn doctor() -> Result<i32> {
    let root = root::detect(None)?;
    eprintln!("root: {}", root.display());
    if root.starts_with(home_child("Documents"))
        || root.starts_with(home_child("Desktop"))
        || root.starts_with(home_child("Downloads"))
    {
        eprintln!(
            "warning:\n  macOS privacy controls may affect child process or snapshot access."
        );
    }
    Ok(0)
}

fn inspect(args: &[String]) -> Result<i32> {
    if args != ["--json"] {
        bail!("inspect currently requires --json");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&crate::json::inspect_report())?
    );
    Ok(0)
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

fn increment(counts: &mut BTreeMap<String, usize>, key: &str) {
    *counts.entry(key.to_owned()).or_default() += 1;
}

fn print_human_report(report: &RunReport) {
    eprintln!(
        "\ntxpt {}\n\nroot:\n  {}\n\nsnapshot:\n  engine: {}\n\ncommand:\n  {}\n\nexit:\n  {}\n\nchanges:\n  modified  {}\n  created   {}\n  deleted   {}\n\nrollback:\n  full          {}\n  cleanup_only {}\n  unprotected  {}\n\nnext:\n  txpt diff\n  txpt undo",
        report.id,
        report.root,
        report.snapshot_engine,
        report.command.join(" "),
        report.child_exit_code,
        report.changes["modified"],
        report.changes["created"],
        report.changes["deleted"],
        report.rollback.get("full").copied().unwrap_or(0),
        report.rollback.get("cleanup_only").copied().unwrap_or(0),
        report.rollback.get("unprotected").copied().unwrap_or(0),
    );
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
    format!("unix:{}", storage::tx_id())
}

fn home_child(name: &str) -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(name)
}

fn print_help() {
    eprintln!(
        "txpt creates reversible transaction points around Unix commands\n\nusage:\n  txpt -- <cmd> [args...]\n  txpt run [options] -- <cmd> [args...]\n  txpt run [options] --shell '<shell command>'\n  txpt diff [TX_ID]\n  txpt undo [TX_ID] [--dry-run] [--force] [--json]\n  txpt list\n  txpt show [TX_ID]\n  txpt prune\n  txpt doctor\n  txpt inspect --json"
    );
}

fn unsafe_getuid() -> u32 {
    // SAFETY: getuid has no preconditions and returns the process uid.
    unsafe { libc::getuid() }
}

fn unsafe_getgid() -> u32 {
    // SAFETY: getgid has no preconditions and returns the process gid.
    unsafe { libc::getgid() }
}
