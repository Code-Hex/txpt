use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::root;
use crate::storage;

use super::{history, shims};

pub(crate) fn start() -> Result<i32> {
    if env::var_os("TXPT_ACTIVE").is_some() {
        eprintln!(
            "txpt: refusing nested session\n\nreason:\n  a txpt session is already active.\n\nuse:\n  txpt\n  txpt ls\n  txpt diff\n  txpt undo"
        );
        return Ok(64);
    }
    let root = root::detect(None)?;
    let state = storage::init_state(&root)?;
    let id = storage::tx_id();
    let session_dir = state.join("sessions").join(&id);
    let bin_dir = session_dir.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let policy = shims::default_session_policy();
    storage::write_json(&session_dir.join("policy.json"), &policy)?;
    shims::write_shims(&bin_dir, &policy)?;

    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    let shell_name = shell_name(&shell).to_owned();
    let new_path = session_path(&bin_dir)?;
    prepare_shell_startup(&session_dir, &shell)?;
    eprintln!("txpt session started");
    eprintln!("root: {}", root.display());
    eprintln!("mode: protected shell");
    eprintln!("shims: {}", shims::shim_commands(&policy).join(" "));
    eprintln!(
        "note: txpt is not a sandbox; absolute paths, shell redirections, and interpreter-driven file changes are outside session shims."
    );
    let mut command = Command::new(&shell);
    configure_interactive_shell(&mut command, &session_dir, &shell);
    let status = command
        .current_dir(env::current_dir()?)
        .env("PATH", new_path)
        .env("TXPT_ACTIVE", "1")
        .env(
            "TXPT_READY",
            if matches!(shell_name.as_str(), "zsh" | "bash") {
                "0"
            } else {
                "1"
            },
        )
        .env("TXPT_SESSION_ID", &id)
        .env("TXPT_SESSION_DIR", &session_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(status.code().unwrap_or(0))
}

fn session_path(bin_dir: &Path) -> Result<OsString> {
    let mut paths = vec![bin_dir.to_path_buf()];
    if let Some(exe_dir) = env::current_exe()?.parent().map(Path::to_path_buf) {
        add_path(&mut paths, exe_dir);
    }
    for path in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        add_path(&mut paths, path);
    }
    Ok(env::join_paths(paths)?)
}

fn add_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

pub(crate) fn dashboard() -> Result<i32> {
    let root = root::detect(None)?;
    let policy = shims::active_policy().unwrap_or_else(|_| shims::default_session_policy());
    println!("txpt session");
    println!("\nroot:\n  {}", root.display());
    println!(
        "\npolicy:\n  {} protect rules\n  {} ignore rules",
        policy.protect.len(),
        policy.ignore.len()
    );
    if let Ok(session_dir) = shims::active_session_dir() {
        let bin_dir = session_dir.join("bin");
        if !path_starts_with(&bin_dir) {
            println!(
                "\nwarning:\n  txpt shim directory is not first in PATH.\n  run `hash -r` or restart the txpt session."
            );
        }
    }
    println!("\nshims:");
    for command in shims::shim_commands(&policy) {
        println!("  {command}");
    }
    println!(
        "\nnot sandboxed:\n  absolute-path commands, `command <name>`, shell redirections, pipelines, and interpreter-driven file changes"
    );
    if let Some((state, command)) = history::last_point_summary(&root) {
        println!("\nlast point:\n  @last  {state}  {command}");
    }
    println!("\ncommands:\n  txpt diff\n  txpt undo\n  txpt rollback\n  txpt shims ls");
    Ok(0)
}

fn prepare_shell_startup(session_dir: &Path, shell: &str) -> Result<()> {
    let shell_name = shell_name(shell);
    let exe = shims::shell_quote(&env::current_exe()?.display().to_string());
    let zsh_repath = "\
__txpt_exe='__TXPT_EXE__'

__txpt_shell_wrap() {
  emulate -L zsh
  local txpt_cmd=\"$1\"
  shift
  local txpt_orig=\"__txpt_original_$txpt_cmd\"
  if \"$__txpt_exe\" shim-should-wrap \"$txpt_cmd\" \"$@\"; then
    local txpt_bin=\"$TXPT_SESSION_DIR/bin\"
    local txpt_display txpt_id txpt_status txpt_finish txpt_old_path
    txpt_display=$(\"$__txpt_exe\" argv-json \"$txpt_cmd\" \"$@\") || return $?
    txpt_id=$(\"$__txpt_exe\" shim-begin \"$txpt_display\") || return $?
    txpt_old_path=\"$PATH\"
    path=(\"${(@)path:#$txpt_bin}\")
    export PATH
    \"$txpt_orig\" \"$@\"
    txpt_status=$?
    PATH=\"$txpt_old_path\"
    export PATH
    \"$__txpt_exe\" shim-finish \"$txpt_id\" \"$txpt_status\"
    txpt_finish=$?
    (( txpt_finish == 0 )) || return $txpt_finish
    return $txpt_status
  else
    \"$txpt_orig\" \"$@\"
  fi
}

__txpt_capture_function_shim() {
  emulate -L zsh
  local txpt_cmd=\"$1\"
  local txpt_orig=\"__txpt_original_$txpt_cmd\"
  if (( $+functions[$txpt_cmd] )) && [[ \"$functions[$txpt_cmd]\" != *__txpt_shell_wrap* ]]; then
    functions[$txpt_orig]=$functions[$txpt_cmd]
    eval \"$txpt_cmd() { __txpt_shell_wrap ${(q)txpt_cmd} \\\"\\$@\\\"; }\"
  fi
}

__txpt_repath() {
  emulate -L zsh
  local txpt_bin=\"$TXPT_SESSION_DIR/bin\"
  local txpt_cmd
  path=(\"${(@)path:#$txpt_bin}\")
  path=(\"$txpt_bin\" \"${path[@]}\")
  export PATH
  for txpt_cmd in \"$txpt_bin\"/*(N:t); do
    __txpt_capture_function_shim \"$txpt_cmd\"
    unalias \"$txpt_cmd\" 2>/dev/null || true
  done
  hash -r 2>/dev/null || true
}
__txpt_repath
autoload -Uz add-zsh-hook 2>/dev/null || true
add-zsh-hook precmd __txpt_repath 2>/dev/null || true
add-zsh-hook preexec __txpt_repath 2>/dev/null || true
";
    let bash_repath = "\
__txpt_exe='__TXPT_EXE__'

__txpt_shell_wrap() {
  local txpt_cmd=\"$1\"
  shift
  local txpt_orig=\"__txpt_original_$txpt_cmd\"
  if \"$__txpt_exe\" shim-should-wrap \"$txpt_cmd\" \"$@\"; then
    local txpt_bin=\"$TXPT_SESSION_DIR/bin\"
    local txpt_display txpt_id txpt_status txpt_finish txpt_old_path
    txpt_display=$(\"$__txpt_exe\" argv-json \"$txpt_cmd\" \"$@\") || return $?
    txpt_id=$(\"$__txpt_exe\" shim-begin \"$txpt_display\") || return $?
    txpt_old_path=\"$PATH\"
    PATH=\":$PATH:\"
    PATH=${PATH//:$txpt_bin:/:}
    PATH=${PATH#:}
    PATH=${PATH%:}
    export PATH
    \"$txpt_orig\" \"$@\"
    txpt_status=$?
    PATH=\"$txpt_old_path\"
    export PATH
    \"$__txpt_exe\" shim-finish \"$txpt_id\" \"$txpt_status\"
    txpt_finish=$?
    [ \"$txpt_finish\" -eq 0 ] || return \"$txpt_finish\"
    return \"$txpt_status\"
  else
    \"$txpt_orig\" \"$@\"
  fi
}

__txpt_capture_function_shim() {
  local txpt_cmd=\"$1\"
  local txpt_orig=\"__txpt_original_$txpt_cmd\"
  if declare -F \"$txpt_cmd\" >/dev/null && ! declare -f \"$txpt_cmd\" | grep -q __txpt_shell_wrap; then
    eval \"$(declare -f \"$txpt_cmd\" | sed \"1s/^$txpt_cmd[[:space:]]*/$txpt_orig /\")\"
    eval \"$txpt_cmd() { __txpt_shell_wrap '$txpt_cmd' \\\"\\$@\\\"; }\"
  fi
}

__txpt_repath() {
  local txpt_bin=\"$TXPT_SESSION_DIR/bin\"
  local txpt_file txpt_cmd
  local old_path=\":$PATH:\"
  old_path=${old_path//:$txpt_bin:/:}
  old_path=${old_path#:}
  old_path=${old_path%:}
  export PATH=\"$txpt_bin:$old_path\"
  for txpt_file in \"$txpt_bin\"/*; do
    [ -e \"$txpt_file\" ] || continue
    txpt_cmd=${txpt_file##*/}
    __txpt_capture_function_shim \"$txpt_cmd\"
    unalias \"$txpt_cmd\" 2>/dev/null || true
  done
  hash -r 2>/dev/null || true
}
__txpt_repath
case \";${PROMPT_COMMAND:-};\" in
  *\";__txpt_repath;\"*) ;;
  *) PROMPT_COMMAND=\"__txpt_repath${PROMPT_COMMAND:+;$PROMPT_COMMAND}\" ;;
esac
";
    match shell_name {
        "zsh" => {
            fs::write(
                session_dir.join(".zshenv"),
                "[ -f \"$HOME/.zshenv\" ] && . \"$HOME/.zshenv\"\n",
            )?;
            fs::write(
                session_dir.join(".zshrc"),
                format!(
                    "[ -f \"$HOME/.zshrc\" ] && . \"$HOME/.zshrc\"\n{}export TXPT_READY=1\nexport PS1=\"(txpt) $PS1\"\n",
                    zsh_repath.replace("__TXPT_EXE__", &exe)
                ),
            )?;
        }
        "bash" => {
            fs::write(
                session_dir.join(".bashrc"),
                format!(
                    "[ -f \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n{}export TXPT_READY=1\nexport PS1=\"(txpt) $PS1\"\n",
                    bash_repath.replace("__TXPT_EXE__", &exe)
                ),
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn configure_interactive_shell(command: &mut Command, session_dir: &Path, shell: &str) {
    let shell_name = shell_name(shell);
    match shell_name {
        "zsh" => {
            command.arg("-i").env("ZDOTDIR", session_dir);
        }
        "bash" => {
            command
                .arg("--rcfile")
                .arg(session_dir.join(".bashrc"))
                .arg("-i");
        }
        _ => {
            command.arg("-i");
        }
    };
}

fn shell_name(shell: &str) -> &str {
    Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}

fn path_starts_with(bin_dir: &Path) -> bool {
    env::var_os("PATH")
        .and_then(|path| env::split_paths(&path).next())
        .is_some_and(|first| first == bin_dir)
}
