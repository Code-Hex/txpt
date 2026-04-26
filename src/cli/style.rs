use std::io::IsTerminal;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Style {
    enabled: bool,
}

impl Style {
    pub(crate) fn stdout() -> Self {
        Self {
            enabled: color_enabled(std::io::stdout().is_terminal()),
        }
    }

    pub(crate) fn stderr() -> Self {
        Self {
            enabled: color_enabled(std::io::stderr().is_terminal()),
        }
    }

    pub(crate) fn paint(self, code: &str, text: impl AsRef<str>) -> String {
        let text = text.as_ref();
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub(crate) fn bold(self, text: impl AsRef<str>) -> String {
        self.paint("1", text)
    }

    pub(crate) fn dim(self, text: impl AsRef<str>) -> String {
        self.paint("2", text)
    }

    pub(crate) fn red(self, text: impl AsRef<str>) -> String {
        self.paint("31", text)
    }

    pub(crate) fn green(self, text: impl AsRef<str>) -> String {
        self.paint("32", text)
    }

    pub(crate) fn yellow(self, text: impl AsRef<str>) -> String {
        self.paint("33", text)
    }

    pub(crate) fn magenta(self, text: impl AsRef<str>) -> String {
        self.paint("35", text)
    }

    pub(crate) fn cyan(self, text: impl AsRef<str>) -> String {
        self.paint("36", text)
    }
}

fn color_enabled(is_terminal: bool) -> bool {
    if let Some(value) = std::env::var_os("CLICOLOR_FORCE") {
        return value != "0";
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    is_terminal
}

pub(crate) fn state(style: Style, label: &str) -> String {
    match label.trim() {
        "undoable" => style.green(label),
        "partial" | "record-only" => style.yellow(label),
        "conflict" | "broken" => style.red(label),
        "reverted" => style.dim(label),
        _ => label.to_owned(),
    }
}

pub(crate) fn status_letter(style: Style, letter: &str) -> String {
    match letter {
        "A" => style.green(letter),
        "M" => style.yellow(letter),
        "D" => style.red(letter),
        "!" => style.magenta(letter),
        _ => letter.to_owned(),
    }
}

pub(crate) fn patch_line(style: Style, line: &str) -> String {
    if line.starts_with("@@") {
        style.cyan(line)
    } else if line.starts_with("+++") || line.starts_with("---") {
        style.bold(line)
    } else if line.starts_with('+') {
        style.green(line)
    } else if line.starts_with('-') {
        style.red(line)
    } else {
        line.to_owned()
    }
}
