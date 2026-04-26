use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::diff::{ChangeEntry, ChangeKind};
use crate::manifest::{self, EntryType, ManifestEntry};
use crate::storage::TxPaths;

#[derive(Debug, Serialize)]
pub struct RollbackResult {
    pub restored: usize,
    pub removed: usize,
    pub conflicts: usize,
    pub dry_run: bool,
}

pub fn undo(root: &Path, paths: &TxPaths, dry_run: bool, force: bool) -> Result<RollbackResult> {
    let before = manifest::read_jsonl(&paths.tx_dir.join("before.manifest.jsonl"))?;
    let after = manifest::read_jsonl(&paths.tx_dir.join("after.manifest.jsonl"))?;
    let changes = crate::diff::read_jsonl(&paths.tx_dir.join("changes.jsonl"))?;
    let before = map_by_path(&before);
    let after = map_by_path(&after);

    let mut result = RollbackResult {
        restored: 0,
        removed: 0,
        conflicts: 0,
        dry_run,
    };
    let conflict_root = paths.conflicts_dir.join(crate::storage::tx_id());

    for change in ordered_changes(changes) {
        if change.kind == ChangeKind::Unprotected {
            continue;
        }
        let current = current_entry(root, &change.path)?;
        let recorded_after = after.get(&change.path);
        if !matches_after(current.as_ref(), recorded_after) {
            result.conflicts += 1;
            if !force {
                continue;
            }
            if !dry_run {
                preserve_conflict(root, &conflict_root, &change.path)?;
            }
        }

        match change.kind {
            ChangeKind::CreatedFile | ChangeKind::CreatedDir => {
                if !dry_run {
                    remove_path(root, &change.path)?;
                }
                result.removed += 1;
            }
            ChangeKind::DeletedFile
            | ChangeKind::ModifiedFile
            | ChangeKind::MetadataChanged
            | ChangeKind::TypeChanged => {
                let Some(before_entry) = before.get(&change.path) else {
                    continue;
                };
                if !dry_run {
                    restore_entry(root, &paths.snapshot_dir, before_entry)?;
                }
                result.restored += 1;
            }
            ChangeKind::Unprotected => {}
        }
    }

    let mut log = File::create(paths.tx_dir.join("rollback.log"))?;
    writeln!(
        log,
        "restored={} removed={} conflicts={} dry_run={}",
        result.restored, result.removed, result.conflicts, result.dry_run
    )?;
    if result.conflicts > 0 && !force {
        bail!("rollback conflict");
    }
    Ok(result)
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

fn current_entry(root: &Path, rel: &str) -> Result<Option<ManifestEntry>> {
    let path = root.join(rel);
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

fn preserve_conflict(root: &Path, conflict_root: &Path, rel: &str) -> Result<()> {
    let src = root.join(rel);
    if !src.exists() && fs::symlink_metadata(&src).is_err() {
        return Ok(());
    }
    let dst = conflict_root.join(format!("{rel}.current"));
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let meta = fs::symlink_metadata(&src)?;
    if meta.file_type().is_symlink() {
        symlink(fs::read_link(&src)?, &dst)?;
    } else if meta.is_dir() {
        fs::create_dir_all(&dst)?;
    } else {
        fs::copy(&src, &dst)?;
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
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("refusing path outside transaction root: {rel}");
    }
    Ok(root.join(rel_path))
}

fn map_by_path(entries: &[ManifestEntry]) -> BTreeMap<String, ManifestEntry> {
    entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect()
}
