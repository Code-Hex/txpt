use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::diff::{self, ChangeEntry};
use crate::rollback;
use crate::root;
use crate::storage;

use super::{CommandMeta, Meta, tx_state_label};

pub(crate) fn show(args: Vec<String>) -> Result<i32> {
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

pub(crate) fn list(args: Vec<String>) -> Result<i32> {
    let ids_only = args.iter().any(|arg| arg == "--ids");
    if args.iter().any(|arg| arg != "--ids") {
        bail!("unknown list option");
    }
    let root = root::detect(None)?;
    let ids = match storage::list_active_tx_ids(&root) {
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

pub(crate) fn prune() -> Result<i32> {
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

pub(crate) fn last_point_summary(root: &std::path::Path) -> Option<(String, String)> {
    let paths = storage::existing_tx_paths(root, None).ok()?;
    let view = tx_view(root, &paths, Some("@last")).ok()?;
    Some((
        tx_state_label(&view.plan.state).to_owned(),
        view.command.argv.join(" "),
    ))
}
