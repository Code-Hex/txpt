use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::diff::{ChangeEntry, ChangeKind};
use crate::manifest::{self, EntryType, ManifestEntry};
use crate::storage::TxPaths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TxState {
    Undoable,
    Partial,
    Conflict,
    Reverted,
    RecordOnly,
    Broken,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RollbackAction {
    RestorePreimage,
    RemoveCreatedPath,
    RemoveCreatedTree,
    RestoreMetadata,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RollbackStatus {
    Ready,
    Conflict,
    Unprotected,
    Unsupported,
    MissingSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackPlanEntry {
    pub path: String,
    pub kind: ChangeKind,
    pub action: RollbackAction,
    pub status: RollbackStatus,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RollbackSummary {
    pub restorable: usize,
    pub removable: usize,
    pub conflicts: usize,
    pub unprotected: usize,
    pub unsupported: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackPlan {
    pub tx_id: String,
    pub state: TxState,
    pub entries: Vec<RollbackPlanEntry>,
    pub summary: RollbackSummary,
}

#[derive(Debug, Serialize)]
pub struct RollbackResult {
    pub restored: usize,
    pub removed: usize,
    pub conflicts: usize,
    pub dry_run: bool,
}

pub fn plan(root: &Path, paths: &TxPaths) -> RollbackPlan {
    match build_plan(root, paths) {
        Ok(mut plan) => {
            if paths.tx_dir.join("rollback.json").exists() {
                plan.state = TxState::Reverted;
            }
            plan
        }
        Err(err) => RollbackPlan {
            tx_id: tx_id_from_paths(paths),
            state: TxState::Broken,
            entries: vec![RollbackPlanEntry {
                path: String::new(),
                kind: ChangeKind::Unprotected,
                action: RollbackAction::None,
                status: RollbackStatus::Unsupported,
                reason: Some(err.to_string()),
            }],
            summary: RollbackSummary {
                unsupported: 1,
                ..RollbackSummary::default()
            },
        },
    }
}

pub fn undo(root: &Path, paths: &TxPaths, dry_run: bool, force: bool) -> Result<RollbackResult> {
    let plan = plan(root, paths);
    apply_plan(root, paths, &plan, dry_run, force)
}

pub fn apply_plan(
    root: &Path,
    paths: &TxPaths,
    _planned: &RollbackPlan,
    dry_run: bool,
    force: bool,
) -> Result<RollbackResult> {
    let fresh_plan = plan(root, paths);
    let plan = &fresh_plan;
    if matches!(plan.state, TxState::Broken | TxState::RecordOnly) {
        bail!("rollback unsupported");
    }
    if plan.state == TxState::Reverted {
        return Ok(RollbackResult {
            restored: 0,
            removed: 0,
            conflicts: 0,
            dry_run,
        });
    }
    if plan.summary.conflicts > 0 && !force {
        bail!("rollback conflict");
    }

    let before = manifest::read_jsonl(&paths.tx_dir.join("before.manifest.jsonl"))?;
    let before = map_by_path(&before);
    let mut result = RollbackResult {
        restored: 0,
        removed: 0,
        conflicts: plan.summary.conflicts,
        dry_run,
    };
    let conflict_root = paths.conflicts_dir.join(crate::storage::tx_id());

    for entry in ordered_plan_entries(plan.entries.clone()) {
        if matches!(
            entry.status,
            RollbackStatus::Unprotected
                | RollbackStatus::Unsupported
                | RollbackStatus::MissingSnapshot
        ) {
            continue;
        }
        if entry.status == RollbackStatus::Conflict && force && !dry_run {
            preserve_conflict(root, &conflict_root, &entry.path)?;
        }
        match entry.action {
            RollbackAction::RemoveCreatedPath | RollbackAction::RemoveCreatedTree => {
                if !dry_run {
                    remove_path(root, &entry.path)?;
                }
                result.removed += 1;
            }
            RollbackAction::RestorePreimage | RollbackAction::RestoreMetadata => {
                let Some(before_entry) = before.get(&entry.path) else {
                    continue;
                };
                if !dry_run {
                    restore_entry(root, &paths.snapshot_dir, before_entry)?;
                }
                result.restored += 1;
            }
            RollbackAction::None => {}
        }
    }

    if !dry_run {
        crate::storage::remove_tx_dir(paths)?;
    }
    Ok(result)
}

fn build_plan(root: &Path, paths: &TxPaths) -> Result<RollbackPlan> {
    if snapshot_engine(paths)? == "record-only" {
        let changes = crate::diff::read_jsonl(&paths.tx_dir.join("changes.jsonl"))?;
        let entries = ordered_changes(changes)
            .into_iter()
            .map(|change| RollbackPlanEntry {
                path: change.path,
                kind: change.kind,
                action: RollbackAction::None,
                status: RollbackStatus::Unsupported,
                reason: Some("record-only point was created with --snapshot off".to_owned()),
            })
            .collect();
        return Ok(RollbackPlan {
            tx_id: tx_id_from_paths(paths),
            state: TxState::RecordOnly,
            entries,
            summary: RollbackSummary::default(),
        });
    }
    let after = manifest::read_jsonl(&paths.tx_dir.join("after.manifest.jsonl"))?;
    let changes = crate::diff::read_jsonl(&paths.tx_dir.join("changes.jsonl"))?;
    let after = map_by_path(&after);

    let mut entries = Vec::new();
    let mut summary = RollbackSummary::default();

    for change in ordered_changes(changes) {
        if change.kind == ChangeKind::Unprotected {
            summary.unprotected += 1;
            entries.push(RollbackPlanEntry {
                path: change.path,
                kind: change.kind,
                action: RollbackAction::None,
                status: RollbackStatus::Unprotected,
                reason: Some("path was not protected by this transaction".to_owned()),
            });
            continue;
        }
        let current = current_entry(root, &change.path)?;
        let recorded_after = after.get(&change.path);
        let mut status = RollbackStatus::Ready;
        let mut reason = None;
        if requires_snapshot(&change.kind) && !snapshot_exists(paths, &change.path) {
            status = RollbackStatus::MissingSnapshot;
            reason = Some("preimage snapshot is missing".to_owned());
        } else if !matches_after(current.as_ref(), recorded_after) {
            status = RollbackStatus::Conflict;
            reason = Some("current path changed after this transaction".to_owned());
        } else if change.kind == ChangeKind::CreatedDir
            && !created_tree_is_unchanged(root, &after, &change.path)?
        {
            status = RollbackStatus::Conflict;
            reason = Some("created directory subtree changed after this transaction".to_owned());
        }

        let action = action_for(&change.kind);
        match status {
            RollbackStatus::Ready => match action {
                RollbackAction::RemoveCreatedPath | RollbackAction::RemoveCreatedTree => {
                    summary.removable += 1;
                }
                RollbackAction::RestorePreimage | RollbackAction::RestoreMetadata => {
                    summary.restorable += 1;
                }
                RollbackAction::None => {}
            },
            RollbackStatus::Conflict => {
                summary.conflicts += 1;
            }
            RollbackStatus::Unprotected => summary.unprotected += 1,
            RollbackStatus::Unsupported | RollbackStatus::MissingSnapshot => {
                summary.unsupported += 1
            }
        }
        entries.push(RollbackPlanEntry {
            path: change.path,
            kind: change.kind,
            action,
            status,
            reason,
        });
    }

    let state = if summary.conflicts > 0 {
        TxState::Conflict
    } else if summary.unprotected > 0 || summary.unsupported > 0 {
        TxState::Partial
    } else {
        TxState::Undoable
    };
    Ok(RollbackPlan {
        tx_id: tx_id_from_paths(paths),
        state,
        entries,
        summary,
    })
}

fn snapshot_engine(paths: &TxPaths) -> Result<String> {
    let meta: serde_json::Value = crate::storage::read_json(&paths.tx_dir.join("meta.json"))?;
    Ok(meta
        .get("snapshot_engine")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned())
}

fn requires_snapshot(kind: &ChangeKind) -> bool {
    matches!(
        kind,
        ChangeKind::DeletedFile
            | ChangeKind::ModifiedFile
            | ChangeKind::MetadataChanged
            | ChangeKind::TypeChanged
    )
}

fn snapshot_exists(paths: &TxPaths, rel: &str) -> bool {
    checked_relative_path(rel)
        .map(|rel| paths.snapshot_dir.join(rel).exists())
        .unwrap_or(false)
}

fn ordered_changes(mut changes: Vec<ChangeEntry>) -> Vec<ChangeEntry> {
    changes.sort_by(|left, right| match (&left.kind, &right.kind) {
        (
            ChangeKind::CreatedFile | ChangeKind::CreatedDir,
            ChangeKind::CreatedFile | ChangeKind::CreatedDir,
        ) => right.path.cmp(&left.path),
        _ => left.path.cmp(&right.path),
    });
    changes
}

fn ordered_plan_entries(mut entries: Vec<RollbackPlanEntry>) -> Vec<RollbackPlanEntry> {
    entries.sort_by(|left, right| match (&left.action, &right.action) {
        (
            RollbackAction::RemoveCreatedPath | RollbackAction::RemoveCreatedTree,
            RollbackAction::RemoveCreatedPath | RollbackAction::RemoveCreatedTree,
        ) => right.path.cmp(&left.path),
        _ => left.path.cmp(&right.path),
    });
    entries
}

fn action_for(kind: &ChangeKind) -> RollbackAction {
    match kind {
        ChangeKind::CreatedFile => RollbackAction::RemoveCreatedPath,
        ChangeKind::CreatedDir => RollbackAction::RemoveCreatedTree,
        ChangeKind::DeletedFile | ChangeKind::ModifiedFile | ChangeKind::TypeChanged => {
            RollbackAction::RestorePreimage
        }
        ChangeKind::MetadataChanged => RollbackAction::RestoreMetadata,
        ChangeKind::Unprotected => RollbackAction::None,
    }
}

fn current_entry(root: &Path, rel: &str) -> Result<Option<ManifestEntry>> {
    let path = checked_path(root, rel)?;
    if !path.exists() && fs::symlink_metadata(&path).is_err() {
        return Ok(None);
    }
    Ok(Some(manifest::entry_for(root, rel, true)?))
}

fn matches_after(current: Option<&ManifestEntry>, after: Option<&ManifestEntry>) -> bool {
    match (current, after) {
        (None, None) => true,
        (Some(current), Some(after)) => current.comparable_state() == after.comparable_state(),
        _ => false,
    }
}

fn created_tree_is_unchanged(
    root: &Path,
    after: &BTreeMap<String, ManifestEntry>,
    rel: &str,
) -> Result<bool> {
    let after_tree = manifest_subtree(after, rel);
    let current_tree = current_subtree(root, rel)?;
    Ok(after_tree == current_tree)
}

fn manifest_subtree(
    manifest: &BTreeMap<String, ManifestEntry>,
    rel: &str,
) -> BTreeMap<String, String> {
    let prefix = format!("{rel}/");
    manifest
        .iter()
        .filter(|(path, _)| *path == rel || path.starts_with(&prefix))
        .map(|(path, entry)| (path.clone(), entry.comparable_state()))
        .collect()
}

fn current_subtree(root: &Path, rel: &str) -> Result<BTreeMap<String, String>> {
    let base = checked_path(root, rel)?;
    if !base.exists() {
        return Ok(BTreeMap::new());
    }
    let mut paths = BTreeSet::new();
    paths.insert(rel.to_owned());
    for entry in walkdir::WalkDir::new(&base).follow_links(false) {
        let entry = entry?;
        if entry.path() == base {
            continue;
        }
        paths.insert(manifest::rel_string(root, entry.path())?);
    }

    let mut entries = BTreeMap::new();
    for path in paths {
        let entry = manifest::entry_for(root, &path, true)?;
        entries.insert(path, entry.comparable_state());
    }
    Ok(entries)
}

fn preserve_conflict(root: &Path, conflict_root: &Path, rel: &str) -> Result<()> {
    let rel_path = checked_relative_path(rel)?;
    let src = root.join(rel_path);
    if !src.exists() && fs::symlink_metadata(&src).is_err() {
        return Ok(());
    }
    let dst = conflict_root.join(format!("{}.current", rel_path.to_string_lossy()));
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let meta = fs::symlink_metadata(&src)?;
    if meta.file_type().is_symlink() {
        symlink(fs::read_link(&src)?, &dst)?;
    } else if meta.is_dir() {
        copy_dir_preserving_symlinks(&src, &dst)?;
    } else {
        fs::copy(&src, &dst)?;
    }
    Ok(())
}

fn copy_dir_preserving_symlinks(src: &Path, dst: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let target = dst.join(rel);
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.file_type().is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            symlink(fs::read_link(entry.path())?, &target)?;
        } else if meta.is_dir() {
            fs::create_dir_all(&target)?;
        } else if meta.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn remove_path(root: &Path, rel: &str) -> Result<()> {
    let path = checked_path(root, rel)?;
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn restore_entry(root: &Path, snapshot_root: &Path, entry: &ManifestEntry) -> Result<()> {
    let target = checked_path(root, &entry.path)?;
    let snap = snapshot_root.join(&entry.path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    match entry.entry_type {
        EntryType::File => atomic_copy(&snap, &target, entry.mode)?,
        EntryType::Symlink => {
            let _ = fs::remove_file(&target);
            symlink(
                entry
                    .symlink_target
                    .as_ref()
                    .context("missing symlink target in snapshot metadata")?,
                &target,
            )?;
        }
        EntryType::Dir => fs::create_dir_all(&target)?,
        EntryType::Other | EntryType::Missing => {}
    }
    Ok(())
}

fn atomic_copy(src: &Path, target: &Path, mode: u32) -> Result<()> {
    let parent = target.parent().context("target has no parent")?;
    let tmp = parent.join(format!(".txpt-restore-{}", crate::storage::tx_id()));
    fs::copy(src, &tmp).with_context(|| format!("failed to restore {}", target.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, target)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn checked_path(root: &Path, rel: &str) -> Result<PathBuf> {
    Ok(root.join(checked_relative_path(rel)?))
}

fn checked_relative_path(rel: &str) -> Result<&Path> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("refusing path outside transaction root: {rel}");
    }
    Ok(rel_path)
}

fn map_by_path(entries: &[ManifestEntry]) -> BTreeMap<String, ManifestEntry> {
    entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect()
}

fn tx_id_from_paths(paths: &TxPaths) -> String {
    paths
        .tx_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}
