use std::fs;

use anyhow::{Result, bail};

use crate::diff::{self, ChangeEntry};
use crate::rollback;
use crate::root;
use crate::storage;

use super::tx_state_label;

pub(crate) fn show_diff(args: Vec<String>) -> Result<i32> {
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
