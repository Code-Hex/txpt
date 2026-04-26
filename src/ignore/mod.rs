use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Policy {
    root: PathBuf,
    include_ignored: bool,
    include_sensitive: bool,
    ignore_names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    Protected,
    Ignored,
    Sensitive,
    Unsupported,
}

impl Policy {
    pub fn load(root: &Path, include_ignored: bool, include_sensitive: bool) -> Self {
        let mut ignore_names = vec![
            ".txpt".to_owned(),
            ".git".to_owned(),
            "node_modules".to_owned(),
            "target".to_owned(),
            "dist".to_owned(),
            "build".to_owned(),
            ".venv".to_owned(),
            "__pycache__".to_owned(),
            ".cache".to_owned(),
            "vendor".to_owned(),
            ".terraform".to_owned(),
            "tmp".to_owned(),
            "logs".to_owned(),
        ];
        for file in [".txptignore", ".gitignore", ".ignore"] {
            if let Ok(text) = fs::read_to_string(root.join(file)) {
                for line in text.lines().map(str::trim) {
                    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                        continue;
                    }
                    ignore_names.push(line.trim_end_matches('/').to_owned());
                }
            }
        }
        Self {
            root: root.to_path_buf(),
            include_ignored,
            include_sensitive,
            ignore_names,
        }
    }

    pub fn classify(&self, path: &Path) -> Protection {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        if is_sensitive(rel) && !self.include_sensitive {
            return Protection::Sensitive;
        }
        if self.is_ignored(rel) && !self.include_ignored {
            return Protection::Ignored;
        }
        Protection::Protected
    }

    pub fn should_descend(&self, path: &Path) -> bool {
        if path == self.root {
            return true;
        }
        !matches!(
            self.classify(path),
            Protection::Ignored | Protection::Sensitive
        )
    }

    fn is_ignored(&self, rel: &Path) -> bool {
        rel.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            self.ignore_names.iter().any(|pattern| pattern == &name)
        }) || self
            .ignore_names
            .iter()
            .any(|pattern| rel_matches(rel, pattern))
    }
}

fn rel_matches(rel: &Path, pattern: &str) -> bool {
    let text = rel.to_string_lossy();
    text == pattern || text.starts_with(&format!("{pattern}/"))
}

fn is_sensitive(rel: &Path) -> bool {
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
