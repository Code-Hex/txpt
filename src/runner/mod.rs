use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub pid: Option<u32>,
    pub exit_code: i32,
}

pub fn run_child(
    argv: &[String],
    cwd: &Path,
    stdout_log: &Path,
    stderr_log: &Path,
    stream: bool,
) -> Result<RunOutput> {
    if argv.is_empty() {
        bail!("missing command");
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv[0]))?;
    let pid = Some(child.id());
    let mut child_stdout = child.stdout.take().context("missing child stdout pipe")?;
    let mut child_stderr = child.stderr.take().context("missing child stderr pipe")?;
    let stdout_path = stdout_log.to_path_buf();
    let stderr_path = stderr_log.to_path_buf();
    let stdout_thread =
        thread::spawn(move || copy_stream(&mut child_stdout, &stdout_path, stream, true));
    let stderr_thread =
        thread::spawn(move || copy_stream(&mut child_stderr, &stderr_path, stream, false));
    let status = child.wait()?;
    stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stdout thread panicked"))??;
    stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stderr thread panicked"))??;
    Ok(RunOutput {
        pid,
        exit_code: status.code().unwrap_or(1),
    })
}

fn copy_stream<R: Read>(reader: &mut R, log_path: &Path, stream: bool, stdout: bool) -> Result<()> {
    let mut log = File::create(log_path)?;
    let mut buf = [0_u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        log.write_all(&buf[..n])?;
        if stream {
            if stdout {
                std::io::stdout().write_all(&buf[..n])?;
                std::io::stdout().flush()?;
            } else {
                std::io::stderr().write_all(&buf[..n])?;
                std::io::stderr().flush()?;
            }
        }
    }
    Ok(())
}
