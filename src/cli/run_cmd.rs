use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::diff;
use crate::ignore::Policy;
use crate::manifest;
use crate::rollback;
use crate::root;
use crate::runner;
use crate::snapshot::{self, SnapshotMode};
use crate::storage;

use super::{
    CommandMeta, Meta, build_report, print_run_receipt, rollback_guarantee, timestamp,
    unsafe_getgid, unsafe_getuid,
};

pub(crate) fn run(args: Vec<String>) -> Result<i32> {
    run_command(parse_run(args)?)
}

#[derive(Debug)]
pub(crate) struct RunOptions {
    pub(crate) root: Option<PathBuf>,
    pub(crate) snapshot: SnapshotMode,
    pub(crate) json: bool,
    pub(crate) stream: bool,
    pub(crate) include_ignored: bool,
    pub(crate) include_sensitive: bool,
    pub(crate) strict: bool,
    pub(crate) keep: bool,
    pub(crate) shell_command: Option<String>,
    pub(crate) display_argv: Option<Vec<String>>,
    pub(crate) argv: Vec<String>,
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

pub(crate) fn run_command(opts: RunOptions) -> Result<i32> {
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
    pub(crate) argv: Vec<String>,
    pub(crate) display_argv: Vec<String>,
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

pub(crate) fn parse_display_command(value: &str) -> Result<Vec<String>> {
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
