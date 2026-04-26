use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::manifest::{EntryType, ManifestEntry};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    CreatedFile,
    CreatedDir,
    DeletedFile,
    ModifiedFile,
    MetadataChanged,
    TypeChanged,
    Unprotected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEntry {
    pub path: String,
    pub kind: ChangeKind,
    pub rollback: String,
    pub guarantee: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    #[serde(default)]
    pub before_size: Option<u64>,
    #[serde(default)]
    pub after_size: Option<u64>,
    #[serde(default)]
    pub text_diff_available: bool,
}

pub fn diff(before: &[ManifestEntry], after: &[ManifestEntry]) -> Vec<ChangeEntry> {
    let before = map_by_path(before);
    let after = map_by_path(after);
    let paths = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changes = Vec::new();
    for path in paths {
        let old = before.get(&path);
        let new = after.get(&path);
        let Some(change) = diff_one(&path, old, new) else {
            continue;
        };
        changes.push(change);
    }
    changes
}

pub fn mark_record_only(changes: &mut [ChangeEntry]) {
    for change in changes {
        change.rollback = "none".to_owned();
        change.guarantee = "record_only".to_owned();
        change.text_diff_available = false;
    }
}

fn diff_one(
    path: &str,
    old: Option<&ManifestEntry>,
    new: Option<&ManifestEntry>,
) -> Option<ChangeEntry> {
    if old.is_some_and(|entry| !entry.protected) || new.is_some_and(|entry| !entry.protected) {
        let old_state = old.map(ManifestEntry::observable_state);
        let new_state = new.map(ManifestEntry::observable_state);
        if old_state != new_state {
            return Some(ChangeEntry {
                path: path.to_owned(),
                kind: ChangeKind::Unprotected,
                rollback: "none".to_owned(),
                guarantee: "unprotected".to_owned(),
                before_hash: old.and_then(|entry| entry.hash.clone()),
                after_hash: new.and_then(|entry| entry.hash.clone()),
                before_size: old.map(|entry| entry.size),
                after_size: new.map(|entry| entry.size),
                text_diff_available: false,
            });
        }
        return None;
    }

    match (old, new) {
        (None, Some(new)) => {
            let (kind, guarantee, rollback) = match new.entry_type {
                EntryType::Dir => ("created_dir", "cleanup_only", "remove_created_tree"),
                EntryType::File | EntryType::Symlink => {
                    ("created_file", "cleanup_only", "remove_created_path")
                }
                _ => ("type_changed", "unsupported", "none"),
            };
            Some(ChangeEntry {
                path: path.to_owned(),
                kind: serde_json::from_value(serde_json::json!(kind)).ok()?,
                rollback: rollback.to_owned(),
                guarantee: guarantee.to_owned(),
                before_hash: None,
                after_hash: new.hash.clone(),
                before_size: None,
                after_size: Some(new.size),
                text_diff_available: new.entry_type == EntryType::File,
            })
        }
        (Some(old), None) => Some(ChangeEntry {
            path: path.to_owned(),
            kind: ChangeKind::DeletedFile,
            rollback: "restore_preimage".to_owned(),
            guarantee: "full".to_owned(),
            before_hash: old.hash.clone(),
            after_hash: None,
            before_size: Some(old.size),
            after_size: None,
            text_diff_available: old.entry_type == EntryType::File,
        }),
        (Some(old), Some(new)) if old.entry_type != new.entry_type => Some(ChangeEntry {
            path: path.to_owned(),
            kind: ChangeKind::TypeChanged,
            rollback: "restore_preimage".to_owned(),
            guarantee: "full".to_owned(),
            before_hash: old.hash.clone(),
            after_hash: new.hash.clone(),
            before_size: Some(old.size),
            after_size: Some(new.size),
            text_diff_available: old.entry_type == EntryType::File
                && new.entry_type == EntryType::File,
        }),
        (Some(old), Some(new))
            if old.hash != new.hash || old.symlink_target != new.symlink_target =>
        {
            Some(ChangeEntry {
                path: path.to_owned(),
                kind: ChangeKind::ModifiedFile,
                rollback: "restore_preimage".to_owned(),
                guarantee: "full".to_owned(),
                before_hash: old.hash.clone(),
                after_hash: new.hash.clone(),
                before_size: Some(old.size),
                after_size: Some(new.size),
                text_diff_available: old.entry_type == EntryType::File
                    && new.entry_type == EntryType::File,
            })
        }
        (Some(old), Some(new)) if old.mode != new.mode => Some(ChangeEntry {
            path: path.to_owned(),
            kind: ChangeKind::MetadataChanged,
            rollback: "restore_metadata".to_owned(),
            guarantee: "metadata_partial".to_owned(),
            before_hash: old.hash.clone(),
            after_hash: new.hash.clone(),
            before_size: Some(old.size),
            after_size: Some(new.size),
            text_diff_available: false,
        }),
        _ => None,
    }
}

fn map_by_path(entries: &[ManifestEntry]) -> BTreeMap<String, ManifestEntry> {
    entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect()
}

pub fn write_jsonl(path: &Path, changes: &[ChangeEntry]) -> Result<()> {
    let mut file = File::create(path)?;
    for change in changes {
        serde_json::to_writer(&mut file, change)?;
        writeln!(file)?;
    }
    Ok(())
}

pub fn read_jsonl(path: &Path) -> Result<Vec<ChangeEntry>> {
    let file = File::open(path)?;
    let mut changes = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if !line.trim().is_empty() {
            changes.push(serde_json::from_str(&line)?);
        }
    }
    Ok(changes)
}

pub fn write_patch(
    root: &Path,
    snapshot_root: &Path,
    patch_path: &Path,
    changes: &[ChangeEntry],
) -> Result<()> {
    let mut file = File::create(patch_path)?;
    for change in changes {
        match change.kind {
            ChangeKind::ModifiedFile | ChangeKind::DeletedFile | ChangeKind::CreatedFile => {
                if !change.text_diff_available {
                    writeln!(
                        file,
                        "{} {}\n  binary or non-text change, before_size={:?}, after_size={:?}\n",
                        status_letter(&change.kind),
                        change.path,
                        change.before_size,
                        change.after_size
                    )?;
                    continue;
                }
                write_text_patch(root, snapshot_root, &mut file, change)?;
            }
            ChangeKind::CreatedDir => {
                writeln!(file, "A {}/\n", change.path)?;
            }
            ChangeKind::MetadataChanged | ChangeKind::TypeChanged | ChangeKind::Unprotected => {
                writeln!(
                    file,
                    "{} {}\n  rollback: {}\n",
                    status_letter(&change.kind),
                    change.path,
                    change.guarantee
                )?;
            }
        }
    }
    Ok(())
}

fn write_text_patch(
    root: &Path,
    snapshot_root: &Path,
    writer: &mut File,
    change: &ChangeEntry,
) -> Result<()> {
    let before = match change.kind {
        ChangeKind::CreatedFile => String::new(),
        _ => fs::read_to_string(snapshot_root.join(&change.path)).unwrap_or_default(),
    };
    let after = match change.kind {
        ChangeKind::DeletedFile => String::new(),
        _ => fs::read_to_string(root.join(&change.path)).unwrap_or_default(),
    };
    writeln!(writer, "--- before/{}", change.path)?;
    writeln!(writer, "+++ after/{}", change.path)?;
    for line in before.lines() {
        writeln!(writer, "-{line}")?;
    }
    for line in after.lines() {
        writeln!(writer, "+{line}")?;
    }
    writeln!(writer)?;
    Ok(())
}

pub fn status_letter(kind: &ChangeKind) -> &'static str {
    match kind {
        ChangeKind::CreatedFile | ChangeKind::CreatedDir => "A",
        ChangeKind::DeletedFile => "D",
        ChangeKind::ModifiedFile | ChangeKind::MetadataChanged | ChangeKind::TypeChanged => "M",
        ChangeKind::Unprotected => "!",
    }
}
