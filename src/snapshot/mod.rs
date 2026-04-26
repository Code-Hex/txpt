use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::manifest::{EntryType, ManifestEntry};
use crate::platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotMode {
    Auto,
    Clone,
    Copy,
    Off,
}

#[derive(Debug, Clone)]
pub struct SnapshotEngine {
    name: &'static str,
    mode: SnapshotMode,
}

impl SnapshotEngine {
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn probe(root: &Path, state_tmp: &Path, requested: SnapshotMode) -> Result<Self> {
        if requested == SnapshotMode::Off {
            return Ok(Self {
                name: "record-only",
                mode: SnapshotMode::Off,
            });
        }
        fs::create_dir_all(state_tmp)?;
        if requested != SnapshotMode::Copy && clone_probe(state_tmp).is_ok() {
            return Ok(Self {
                name: if cfg!(target_os = "macos") {
                    "apfs-clonefile"
                } else if cfg!(target_os = "linux") {
                    "linux-ficlone"
                } else {
                    "clone"
                },
                mode: SnapshotMode::Clone,
            });
        }
        if requested == SnapshotMode::Clone {
            bail!(
                "clone snapshot requested but unavailable under {}",
                root.display()
            );
        }
        Ok(Self {
            name: "copy",
            mode: SnapshotMode::Copy,
        })
    }

    pub fn snapshot_entry(
        &self,
        root: &Path,
        dst_root: &Path,
        entry: &ManifestEntry,
    ) -> Result<()> {
        if self.mode == SnapshotMode::Off {
            return Ok(());
        }
        let src = root.join(&entry.path);
        let dst = dst_root.join(&entry.path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        match entry.entry_type {
            EntryType::File => {
                if self.mode == SnapshotMode::Clone && platform::clone_file(&src, &dst).is_ok() {
                    return Ok(());
                }
                fs::copy(&src, &dst).with_context(|| {
                    format!(
                        "failed to copy snapshot {} -> {}",
                        src.display(),
                        dst.display()
                    )
                })?;
            }
            EntryType::Symlink => {
                let target = fs::read_link(&src)?;
                symlink(target, &dst)?;
            }
            EntryType::Dir => {
                fs::create_dir_all(&dst)?;
            }
            EntryType::Other | EntryType::Missing => {}
        }
        Ok(())
    }
}

fn clone_probe(tmp: &Path) -> Result<()> {
    let src = tmp.join("probe-src");
    let dst = tmp.join("probe-dst");
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    fs::write(&src, b"before")?;
    platform::clone_file(&src, &dst)?;
    fs::write(&src, b"after")?;
    let dst_text = fs::read(&dst)?;
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    if dst_text == b"before" {
        Ok(())
    } else {
        bail!("clone probe shared writes");
    }
}

pub fn parse_mode(value: &str) -> Result<SnapshotMode> {
    match value {
        "auto" => Ok(SnapshotMode::Auto),
        "clone" => Ok(SnapshotMode::Clone),
        "copy" => Ok(SnapshotMode::Copy),
        "off" => Ok(SnapshotMode::Off),
        _ => bail!("unknown snapshot mode {value}"),
    }
}
