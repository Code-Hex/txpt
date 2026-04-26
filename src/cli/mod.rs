#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
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
        "diff" => show_diff(args[1..].to_vec()),
        "undo" | "rollback" => undo(args[1..].to_vec()),
        "show" => show(args[1..].to_vec()),
        "list" => list(args[1..].to_vec()),
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
    let view = match tx_view(&root, &paths) {
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
        match tx_view(&root, &paths) {
            Ok(view) => {
                println!(
                    "{:<8} {:<10} {:<10} {:<5} {:<12} {}",
                    selector,
                    age(&view.meta.started_at),
                    format!("{:?}", view.plan.state).to_lowercase(),
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

#[derive(Debug, Serialize)]
struct TxView {
    id: String,
    meta: Meta,
    command: CommandMeta,
    changes: Vec<ChangeEntry>,
    plan: rollback::RollbackPlan,
}

fn tx_view(root: &Path, paths: &storage::TxPaths) -> Result<TxView> {
    let meta: Meta = storage::read_json(&paths.tx_dir.join("meta.json"))?;
    let command: CommandMeta = storage::read_json(&paths.tx_dir.join("command.json"))?;
    let changes = diff::read_jsonl(&paths.tx_dir.join("changes.jsonl"))?;
    let plan = rollback::plan(root, paths);
    Ok(TxView {
        id: meta.id.clone(),
        meta,
        command,
        changes,
        plan,
    })
}

fn print_show_card(view: &TxView) {
    println!("txpt @last  {}", view.id);
    println!("\ncommand:\n  {}", view.command.argv.join(" "));
    println!("\ntime:\n  started: {}", view.meta.started_at);
    println!("\nroot:\n  {}", view.meta.root);
    println!("\nsnapshot:\n  engine: {}", view.meta.snapshot_engine);
    println!("\nchanges:");
    for (label, count) in grouped_change_counts(&view.changes) {
        println!("  {label:<11} {count}");
    }
    println!(
        "\nrollback:\n  state: {:?}\n  restorable: {} paths\n  removable: {} paths\n  conflicts: {} paths\n  unprotected: {} paths",
        view.plan.state,
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
    println!("\ncommands:\n  txpt diff @last\n  txpt undo @last --dry-run");
}

fn print_diff_human(
    paths: &storage::TxPaths,
    plan: &rollback::RollbackPlan,
    changes: &[ChangeEntry],
) -> Result<()> {
    println!("txpt diff {}", plan.tx_id);
    println!(
        "\nrollback:\n  state: {:?}\n  can undo: {} paths\n  conflicts: {} paths\n  unprotected: {} paths",
        plan.state,
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
