use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
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

fn diff_one(
    path: &str,
    old: Option<&ManifestEntry>,
    new: Option<&ManifestEntry>,
) -> Option<ChangeEntry> {
    if old.is_some_and(|entry| !entry.protected) || new.is_some_and(|entry| !entry.protected) {
        let old_state = old.map(ManifestEntry::comparable_state);
        let new_state = new.map(ManifestEntry::comparable_state);
        if old_state != new_state {
            return Some(ChangeEntry {
                path: path.to_owned(),
                kind: ChangeKind::Unprotected,
                rollback: "none".to_owned(),
                guarantee: "unprotected".to_owned(),
                before_hash: old.and_then(|entry| entry.hash.clone()),
                after_hash: new.and_then(|entry| entry.hash.clone()),
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
            })
        }
        (Some(old), None) => Some(ChangeEntry {
            path: path.to_owned(),
            kind: ChangeKind::DeletedFile,
            rollback: "restore_preimage".to_owned(),
            guarantee: "full".to_owned(),
            before_hash: old.hash.clone(),
            after_hash: None,
        }),
        (Some(old), Some(new)) if old.entry_type != new.entry_type => Some(ChangeEntry {
            path: path.to_owned(),
            kind: ChangeKind::TypeChanged,
            rollback: "restore_preimage".to_owned(),
            guarantee: "full".to_owned(),
            before_hash: old.hash.clone(),
            after_hash: new.hash.clone(),
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
            })
        }
        (Some(old), Some(new)) if old.mode != new.mode => Some(ChangeEntry {
            path: path.to_owned(),
            kind: ChangeKind::MetadataChanged,
            rollback: "restore_metadata".to_owned(),
            guarantee: "metadata_partial".to_owned(),
            before_hash: old.hash.clone(),
            after_hash: new.hash.clone(),
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
