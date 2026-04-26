use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::ignore::{Policy, Protection};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    File,
    Dir,
    Symlink,
    Other,
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    pub mode: u32,
    pub size: u64,
    pub mtime_ns: i128,
    pub hash: Option<String>,
    pub symlink_target: Option<String>,
    pub sensitive: bool,
    pub protected: bool,
}

impl ManifestEntry {
    pub fn comparable_state(&self) -> String {
        format!(
            "{:?}:{:?}:{:?}:{}",
            self.entry_type, self.hash, self.symlink_target, self.mode
        )
    }

    pub fn observable_state(&self) -> String {
        format!(
            "{:?}:{:?}:{}:{}:{}",
            self.entry_type, self.symlink_target, self.mode, self.size, self.mtime_ns
        )
    }
}

pub fn scan(root: &Path, policy: &Policy) -> Result<Vec<ManifestEntry>> {
    let mut entries = Vec::new();
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry?;
        if entry.path() == root {
            continue;
        }
        let rel = rel_string(root, entry.path())?;
        if rel == ".txpt" || rel.starts_with(".txpt/") || rel == ".git" || rel.starts_with(".git/")
        {
            continue;
        }
        let protection = policy.classify(entry.path());
        if protection == Protection::Unsupported {
            continue;
        }
        entries.push(entry_for(root, &rel, protection == Protection::Protected)?);
        if entry.file_type().is_dir() && !policy.should_descend(entry.path()) {
            walker.skip_current_dir();
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

pub fn entry_for(root: &Path, rel: &str, protected: bool) -> Result<ManifestEntry> {
    let path = root.join(rel);
    let meta = fs::symlink_metadata(&path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let file_type = meta.file_type();
    let entry_type = if file_type.is_file() {
        EntryType::File
    } else if file_type.is_dir() {
        EntryType::Dir
    } else if file_type.is_symlink() {
        EntryType::Symlink
    } else {
        EntryType::Other
    };
    let sensitive = is_sensitive_name(Path::new(rel));
    let hash = if entry_type == EntryType::File && protected && !sensitive {
        Some(format!("blake3:{}", hash_file(&path)?))
    } else {
        None
    };
    let symlink_target = if entry_type == EntryType::Symlink {
        Some(fs::read_link(&path)?.to_string_lossy().into_owned())
    } else {
        None
    };
    Ok(ManifestEntry {
        path: rel.to_owned(),
        entry_type,
        mode: meta.permissions().mode(),
        size: meta.size(),
        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        hash,
        symlink_target,
        sensitive,
        protected,
    })
}

pub fn missing_entry(rel: &str) -> ManifestEntry {
    ManifestEntry {
        path: rel.to_owned(),
        entry_type: EntryType::Missing,
        mode: 0,
        size: 0,
        mtime_ns: 0,
        hash: None,
        symlink_target: None,
        sensitive: false,
        protected: true,
    }
}

pub fn read_jsonl(path: &Path) -> Result<Vec<ManifestEntry>> {
    let file = File::open(path)?;
    let mut entries = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if !line.trim().is_empty() {
            entries.push(serde_json::from_str(&line)?);
        }
    }
    Ok(entries)
}

pub fn write_jsonl(path: &Path, entries: &[ManifestEntry]) -> Result<()> {
    let mut file = File::create(path)?;
    for entry in entries {
        serde_json::to_writer(&mut file, entry)?;
        writeln!(file)?;
    }
    Ok(())
}

pub fn rel_string(root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(root)?;
    Ok(rel
        .components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned())
}

fn is_sensitive_name(rel: &Path) -> bool {
    let Some(name) = rel.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name == ".npmrc"
        || name == ".pypirc"
        || name == ".netrc"
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
