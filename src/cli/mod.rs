#![allow(unsafe_code)]

mod diff_cmd;
mod history;
mod run_cmd;
mod session;
mod shims;
mod style;
mod undo_cmd;

use std::collections::BTreeMap;
use std::env;
use std::io::IsTerminal;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::diff::{self, ChangeEntry};
use crate::rollback;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub(crate) id: String,
    pub(crate) version: u8,
    pub(crate) root: String,
    pub(crate) cwd: String,
    pub(crate) platform: String,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) state_dir: String,
    pub(crate) snapshot_engine: String,
    pub(crate) child_exit_code: i32,
    pub(crate) txpt_status: String,
    pub(crate) rollback_guarantee: String,
    pub(crate) keep: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CommandMeta {
    pub(crate) argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) real_argv: Option<Vec<String>>,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) env_redacted: bool,
    pub(crate) pid: Option<u32>,
    pub(crate) shell: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunReport {
    pub(crate) id: String,
    pub(crate) root: String,
    pub(crate) command: Vec<String>,
    pub(crate) child_exit_code: i32,
    pub(crate) snapshot_engine: String,
    pub(crate) changes: BTreeMap<String, usize>,
    pub(crate) rollback: BTreeMap<String, usize>,
}

pub fn run(args: Vec<String>) -> Result<i32> {
    if args.is_empty() {
        return no_args();
    }
    if args[0] == "--" {
        return run_cmd::run(args[1..].to_vec());
    }
    match args[0].as_str() {
        "run" => run_cmd::run(args[1..].to_vec()),
        "diff" => diff_cmd::show_diff(args[1..].to_vec()),
        "undo" | "rollback" => undo_cmd::undo(args[1..].to_vec()),
        "show" => history::show(args[1..].to_vec()),
        "list" | "ls" => history::list(args[1..].to_vec()),
        "prune" => history::prune(),
        "shims" => shims::shims(args[1..].to_vec()),
        "shim-exec" => shims::shim_exec(args[1..].to_vec()),
        "shim-should-wrap" => shims::shim_should_wrap(args[1..].to_vec()),
        "shim-begin" => shims::shim_begin(args[1..].to_vec()),
        "shim-finish" => shims::shim_finish(args[1..].to_vec()),
        "argv-json" => shims::argv_json(args[1..].to_vec()),
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
        return session::dashboard();
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return session::start();
    }
    print_help();
    Ok(0)
}

pub(crate) fn build_report(
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

pub(crate) fn increment(counts: &mut BTreeMap<String, usize>, key: &str) {
    *counts.entry(key.to_owned()).or_default() += 1;
}

fn receipt_warnings(command: &[String]) -> Vec<&'static str> {
    let Some(program) = command.first().map(String::as_str) else {
        return Vec::new();
    };
    let command_line = command.join(" ");
    let mut warnings = Vec::new();
    if matches!(program, "dd" | "rsync") {
        warnings.push("command may modify files outside the transaction root; txpt only restores protected workspace paths.");
    }
    if program == "git"
        && (command_line.starts_with("git clean")
            || command_line.starts_with("git reset --hard")
            || command_line.starts_with("git restore --staged")
            || command_line.starts_with("git rm --cached")
            || command_line.starts_with("git stash pop")
            || command_line.starts_with("git stash apply"))
    {
        warnings.push("git command may modify .git state; txpt restores workspace files, not Git index, reflog, stash, or repository metadata.");
    }
    warnings
}

pub(crate) fn print_run_receipt(
    report: &RunReport,
    plan: &rollback::RollbackPlan,
    changes: &[ChangeEntry],
) {
    let style = style::Style::stderr();
    eprintln!(
        "\n{} {}  {}\n\n{}:\n  {}\n\n{}:\n  exit {}\n\n{}:",
        style.bold("txpt point"),
        style.cyan("@last"),
        style.cyan(&report.id),
        style.bold("command"),
        report.command.join(" "),
        style.bold("result"),
        report.child_exit_code,
        style.bold("changed"),
    );
    for change in changes.iter().take(12) {
        eprintln!(
            "  {} {}",
            style::status_letter(style, diff::status_letter(&change.kind)),
            change.path
        );
    }
    if changes.len() > 12 {
        eprintln!("  {} {} more", style.dim("..."), changes.len() - 12);
    }
    let warnings = receipt_warnings(&report.command);
    if !warnings.is_empty() {
        eprintln!("\n{}:", style.yellow("warning"));
        for warning in warnings {
            eprintln!("  {warning}");
        }
    }
    eprintln!(
        "\n{}:\n  state: {}\n  will restore: {} paths\n  will remove:  {} paths\n\n{}:\n  inspect: {}\n  preview: {}\n  undo:    {}\n  alias:   {}",
        style.bold("rollback"),
        style::state(style, tx_state_label(&plan.state)),
        plan.summary.restorable,
        plan.summary.removable,
        style.bold("next"),
        style.cyan("txpt diff @last"),
        style.cyan("txpt undo @last --dry-run"),
        style.cyan("txpt undo @last"),
        style.cyan("txpt rollback @last"),
    );
}

pub(crate) fn rollback_guarantee(changes: &[ChangeEntry]) -> String {
    if changes
        .iter()
        .all(|change| change.guarantee == "full" || change.guarantee == "cleanup_only")
    {
        "full".to_owned()
    } else {
        "partial".to_owned()
    }
}

pub(crate) fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

pub(crate) fn tx_state_label(state: &rollback::TxState) -> &'static str {
    match state {
        rollback::TxState::Undoable => "undoable",
        rollback::TxState::Partial => "partial",
        rollback::TxState::Conflict => "conflict",
        rollback::TxState::Reverted => "reverted",
        rollback::TxState::RecordOnly => "record-only",
        rollback::TxState::Broken => "broken",
    }
}

pub(crate) fn unsafe_getuid() -> u32 {
    // SAFETY: getuid has no preconditions and returns the process uid.
    unsafe { libc::getuid() }
}

pub(crate) fn unsafe_getgid() -> u32 {
    // SAFETY: getgid has no preconditions and returns the process gid.
    unsafe { libc::getgid() }
}

fn print_help() {
    eprintln!(
        "txpt creates reversible transaction points around Unix commands\n\nusage:\n  txpt                         # protected shell session when interactive\n  txpt -- <cmd> [args...]\n  txpt run [options] -- <cmd> [args...]\n  txpt run [options] --shell '<shell command>'\n  txpt diff [TX_ID]\n  txpt undo|rollback [TX_ID] [--dry-run] [--force] [--json]\n  txpt list|ls [--ids] [--json]\n  txpt show [TX_ID]\n  txpt shims list|ls|status\n  txpt shims protect <command-pattern>...\n  txpt shims unprotect <command-pattern>...\n  txpt shims ignore <command-pattern>...\n  txpt shims unignore <command-pattern>...\n  txpt shims edit\n  txpt prune"
    );
}
