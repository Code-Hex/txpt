use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

const PROJECT_MARKERS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "Cargo.toml",
    "go.mod",
    "deno.json",
    "bun.lock",
    "pnpm-lock.yaml",
];

const BUILD_MARKERS: &[&str] = &["Makefile", "justfile", "Dockerfile", "docker-compose.yml"];

pub fn detect(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(root) = explicit {
        return root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize root {}", root.display()));
    }

    let cwd = env::current_dir()?;
    if let Some(root) = git_root(&cwd) {
        return Ok(root);
    }
    if let Some(root) = nearest_with(&cwd, &[".txpt-root"]) {
        return Ok(root);
    }
    if let Some(root) = nearest_with(&cwd, PROJECT_MARKERS) {
        return Ok(root);
    }
    if let Some(root) = nearest_with(&cwd, BUILD_MARKERS) {
        return Ok(root);
    }
    Ok(cwd)
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let root = PathBuf::from(text.trim());
    root.canonicalize().ok()
}

fn nearest_with(cwd: &Path, names: &[&str]) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        if names.iter().any(|name| dir.join(name).exists()) {
            return Some(dir.to_path_buf());
        }
    }
    None
}
