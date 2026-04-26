use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::diff;
use crate::ignore::Policy;
use crate::manifest;
use crate::rollback;
use crate::root;
use crate::snapshot::{self, SnapshotMode};
use crate::storage;

use super::run_cmd::parse_display_command;
use super::run_cmd::{self, RunOptions};
use super::{
    CommandMeta, Meta, build_report, print_run_receipt, rollback_guarantee, timestamp,
    unsafe_getgid, unsafe_getuid,
};

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
pub(crate) struct SessionPolicy {
    pub(crate) protect: Vec<String>,
    pub(crate) ignore: Vec<String>,
}
pub(crate) fn shims(args: Vec<String>) -> Result<i32> {
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
    let mut editor = editor_command()?;
    let status = editor
        .arg(&path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to launch editor")?;
    if !status.success() {
        bail!("editor exited with {}", status.code().unwrap_or(1));
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read edited policy {}", path.display()))?;
    let policy = match serde_json::from_str::<SessionPolicy>(&text) {
        Ok(policy) => policy,
        Err(err) => {
            eprintln!(
                "warning:\n  edited policy.json is invalid: {err}\n  shims were not regenerated."
            );
            return Ok(74);
        }
    };
    write_shims(&session_dir.join("bin"), &policy)?;
    Ok(0)
}

fn editor_command() -> Result<Command> {
    if let Some(editor) = env::var_os("EDITOR").filter(|value| !value.is_empty()) {
        return Ok(Command::new(editor));
    }
    for editor in ["vim", "vi", "nano"] {
        if let Some(path) = find_command_in_path(editor) {
            return Ok(Command::new(path));
        }
    }
    bail!("EDITOR is not set and no fallback editor was found (tried vim, vi, nano)")
}

pub(crate) fn shim_exec(args: Vec<String>) -> Result<i32> {
    let Some(command) = args.first() else {
        bail!("shim-exec requires a command");
    };
    let rest = args[1..].to_vec();
    let real = find_real_command(command)?;
    if env::var_os("TXPT_READY").as_deref() == Some(std::ffi::OsStr::new("0")) {
        let status = Command::new(&real)
            .args(rest)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        return Ok(status.code().unwrap_or(1));
    }
    let policy = active_policy()?;
    let mut real_argv = vec![real.display().to_string()];
    real_argv.extend(rest.clone());
    if should_wrap(&policy, command, &rest) {
        let mut display_argv = vec![command.clone()];
        display_argv.extend(rest);
        return run_cmd::run_command(RunOptions {
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

pub(crate) fn shim_should_wrap(args: Vec<String>) -> Result<i32> {
    let Some(command) = args.first() else {
        bail!("shim-should-wrap requires a command");
    };
    if env::var_os("TXPT_READY").as_deref() == Some(std::ffi::OsStr::new("0")) {
        return Ok(1);
    }
    let policy = active_policy()?;
    Ok(if should_wrap(&policy, command, &args[1..]) {
        0
    } else {
        1
    })
}

pub(crate) fn shim_begin(args: Vec<String>) -> Result<i32> {
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

pub(crate) fn shim_finish(args: Vec<String>) -> Result<i32> {
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

pub(crate) fn argv_json(args: Vec<String>) -> Result<i32> {
    if args.is_empty() {
        bail!("argv-json requires at least one argument");
    }
    println!("{}", serde_json::to_string(&args)?);
    Ok(0)
}

pub(crate) fn default_session_policy() -> SessionPolicy {
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
        ],
    }
}

pub(crate) fn write_shims(bin_dir: &Path, policy: &SessionPolicy) -> Result<()> {
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

pub(crate) fn shell_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

pub(crate) fn active_session_dir() -> Result<PathBuf> {
    env::var_os("TXPT_SESSION_DIR")
        .map(PathBuf::from)
        .context("txpt shims requires an active txpt session (TXPT_SESSION_DIR is not set)")
}

pub(crate) fn active_policy() -> Result<SessionPolicy> {
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

pub(crate) fn shim_commands(policy: &SessionPolicy) -> Vec<String> {
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
    if let Some(path) = find_command_in_path(command) {
        return Ok(path);
    }
    bail!("failed to find real command {command}")
}

fn find_command_in_path(command: &str) -> Option<PathBuf> {
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
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
