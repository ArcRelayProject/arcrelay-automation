use crate::*;
use std::{collections::BTreeMap, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

const OUTPUT_LIMIT: usize = 64 * 1024;

async fn capture(mut reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        let take = n.min(OUTPUT_LIMIT.saturating_sub(kept.len()));
        kept.extend_from_slice(&buffer[..take]);
    }
    Ok(kept)
}

/// Event data is passed through environment variables, never pasted into executable code.
fn prepare_script(
    script: &str,
    event: &AutomationEvent,
    powershell: bool,
) -> Result<(String, BTreeMap<String, String>)> {
    let mut output = String::new();
    let mut env = BTreeMap::new();
    let mut rest = script;
    while let Some((head, tail)) = rest.split_once("{{") {
        let (key, next) = tail
            .split_once("}}")
            .ok_or_else(|| AutomationError::Invalid("invalid event variable".into()))?;
        let value = event.variables.get(key.trim()).ok_or_else(|| {
            AutomationError::Invalid(format!(
                "the event does not provide variable {}; select a real activity when testing",
                key.trim()
            ))
        })?;
        let name = format!("ARCRELAY_EVENT_{}", env.len());
        output.push_str(head);
        output.push_str(&if powershell {
            format!("$env:{name}")
        } else {
            format!("${{{name}}}")
        });
        env.insert(name, value.clone());
        rest = next;
    }
    output.push_str(rest);
    Ok((output, env))
}

struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

pub async fn run_shell(
    shell: ShellKind,
    script: &str,
    directory: Option<&str>,
    timeout_seconds: u32,
    event: &AutomationEvent,
    cancel: CancellationToken,
) -> StepResult {
    execute_shell(
        shell,
        script,
        directory,
        timeout_seconds,
        Some(event),
        cancel,
    )
    .await
}

/// Existing quick-action scripts are literal user code, not automation variable templates.
pub async fn run_shell_literal(
    shell: ShellKind,
    script: &str,
    directory: Option<&str>,
    timeout_seconds: u32,
    cancel: CancellationToken,
) -> StepResult {
    execute_shell(shell, script, directory, timeout_seconds, None, cancel).await
}

async fn execute_shell(
    shell: ShellKind,
    script: &str,
    directory: Option<&str>,
    timeout_seconds: u32,
    event: Option<&AutomationEvent>,
    cancel: CancellationToken,
) -> StepResult {
    let execute = async {
        let powershell = shell == ShellKind::Powershell || (shell == ShellKind::System && cfg!(windows));
        let (script, env) = match event {
            Some(event) => prepare_script(script, event, powershell).map_err(|e| e.to_string())?,
            None => (script.to_string(), BTreeMap::new()),
        };
        let program = match shell {
            ShellKind::Powershell => if cfg!(windows) { "powershell.exe".to_string() } else { "pwsh".to_string() },
            ShellKind::System if cfg!(windows) => "powershell.exe".into(),
            ShellKind::System if cfg!(target_os="macos") => std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
            ShellKind::System | ShellKind::Sh => "/bin/sh".into(), ShellKind::Bash => "/bin/bash".into(), ShellKind::Zsh => "/bin/zsh".into(),
        };
        let mut command = Command::new(program);
        if powershell { command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]); } else { command.arg("-c"); }
        command.arg(script).envs(env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        if let Some(directory) = directory.filter(|s| !s.is_empty()) { command.current_dir(directory); }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let group = ProcessGroup(child.id().ok_or("failed to get the script process")?);
        let stdout = tokio::spawn(capture(child.stdout.take().ok_or("failed to capture stdout")?));
        let stderr = tokio::spawn(capture(child.stderr.take().ok_or("failed to capture stderr")?));
        let outcome = tokio::select! {
            result = child.wait() => result.map(|status| (status.code(), if status.success() { None } else { Some(format!("script failed with exit code {}", status.code().map(|v| v.to_string()).unwrap_or_else(|| "terminated by signal".into()))) })).map_err(|e| e.to_string()),
            _ = cancel.cancelled() => Err("run canceled".into()),
            _ = tokio::time::sleep(Duration::from_secs(timeout_seconds.clamp(1,3600) as u64)) => Err("script timed out".into()),
        };
        drop(group); // Also close pipes retained by descendants after the shell exits.
        let _ = child.kill().await; let _ = child.wait().await;
        let collect = |mut task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>| async move {
            match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
                Ok(Ok(Ok(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
                _ => { task.abort(); "[output capture ended]".into() }
            }
        };
        let (out, err) = tokio::join!(collect(stdout), collect(stderr));
        let (exit_code,error) = outcome.unwrap_or_else(|error| (None,Some(error)));
        Ok::<_,String>(StepResult { exit_code, stdout: out, stderr: err, error })
    }.await;
    execute
        .unwrap_or_else(|error| StepResult {
            error: Some(error),
            ..Default::default()
        })
        .bounded()
}
