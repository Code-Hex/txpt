use anyhow::{Result, bail};

use crate::rollback;
use crate::root;
use crate::storage;

pub(crate) fn undo(args: Vec<String>) -> Result<i32> {
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
