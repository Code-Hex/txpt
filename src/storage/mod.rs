use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct TxPaths {
    pub state_dir: PathBuf,
    pub tx_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub conflicts_dir: PathBuf,
}

#[derive(Debug)]
pub struct TxLock {
    path: PathBuf,
}

impl Drop for TxLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn state_dir(root: &Path) -> PathBuf {
    root.join(".txpt")
}

pub fn init_state(root: &Path) -> Result<PathBuf> {
    let state = state_dir(root);
    fs::create_dir_all(state.join("tx"))?;
    fs::create_dir_all(state.join("tmp"))?;
    fs::create_dir_all(state.join("locks"))?;
    fs::create_dir_all(state.join("conflicts"))?;
    Ok(state)
}

pub fn acquire_lock(state: &Path) -> Result<TxLock> {
    let path = state.join("locks").join("txpt.lock");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| "another txpt transaction is already running")?;
    Ok(TxLock { path })
}

pub fn tx_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}.{:09}-{:04x}",
        now.as_secs(),
        now.subsec_nanos(),
        now.subsec_nanos() & 0xffff
    )
}

pub fn tx_paths(root: &Path, id: &str) -> Result<TxPaths> {
    let state = init_state(root)?;
    let tx_dir = state.join("tx").join(id);
    let snapshot_dir = tx_dir.join("snapshot");
    fs::create_dir_all(&snapshot_dir)?;
    Ok(TxPaths {
        tmp_dir: state.join("tmp"),
        conflicts_dir: state.join("conflicts"),
        state_dir: state,
        tx_dir,
        snapshot_dir,
    })
}

pub fn latest_tx_id(root: &Path) -> Result<String> {
    let Some(id) = list_tx_ids(root)?.into_iter().next() else {
        bail!("no transactions recorded");
    };
    Ok(id)
}

pub fn list_tx_ids(root: &Path) -> Result<Vec<String>> {
    let tx_root = state_dir(root).join("tx");
    let mut entries = fs::read_dir(&tx_root)
        .with_context(|| format!("no transaction directory at {}", tx_root.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.retain(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()));
    entries.sort_by(|left, right| {
        let left_modified = left.metadata().and_then(|meta| meta.modified()).ok();
        let right_modified = right.metadata().and_then(|meta| meta.modified()).ok();
        right_modified
            .cmp(&left_modified)
            .then_with(|| right.file_name().cmp(&left.file_name()))
    });
    Ok(entries
        .into_iter()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect())
}

pub fn resolve_tx_id(root: &Path, selector: Option<&str>) -> Result<String> {
    let ids = list_tx_ids(root)?;
    match selector {
        None | Some("@last") => ids
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no transactions recorded")),
        Some(value) if value.starts_with('@') => {
            let index = value
                .trim_start_matches('@')
                .parse::<usize>()
                .with_context(|| format!("invalid transaction selector {value}"))?;
            ids.get(index)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("transaction selector {value} is out of range"))
        }
        Some(id) => Ok(id.to_owned()),
    }
}

pub fn existing_tx_paths(root: &Path, id: Option<&str>) -> Result<TxPaths> {
    let id = resolve_tx_id(root, id)?;
    let state = state_dir(root);
    let tx_dir = state.join("tx").join(id);
    Ok(TxPaths {
        snapshot_dir: tx_dir.join("snapshot"),
        tmp_dir: state.join("tmp"),
        conflicts_dir: state.join("conflicts"),
        state_dir: state,
        tx_dir,
    })
}

pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let file = File::create(path)?;
    serde_json::to_writer_pretty(file, value)?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path)?;
    Ok(serde_json::from_reader(file)?)
}
