use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use uuid::Uuid;

use crate::errors::{AppError, AppResult};
use crate::ssh::{self, ExecFailure, ExecRequest, ExecResult, SshContext};
use crate::util::{escape_for_shell, format_duration};

pub struct SessionManager {
    context: SshContext,
}

impl SessionManager {
    pub fn new(context: SshContext) -> Self {
        Self { context }
    }

    pub async fn open(&self, name: &str, cwd: Option<&str>) -> AppResult<()> {
        validate_name(name)?;
        let mut args = vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            name.to_string(),
        ];
        if let Some(path) = cwd {
            args.push("-c".to_string());
            args.push(path.to_string());
        }
        args.push("${SHELL:-/bin/bash}".to_string());
        run_tmux(&self.context, &args, Some(Duration::from_secs(10)))
            .await
            .map(|_| ())
    }

    pub async fn close(&self, name: &str) -> AppResult<()> {
        validate_name(name)?;
        let args = vec![
            "kill-session".to_string(),
            "-t".to_string(),
            name.to_string(),
        ];
        run_tmux(&self.context, &args, Some(Duration::from_secs(10)))
            .await
            .map(|_| ())
    }

    pub async fn exec<F>(
        &self,
        name: &str,
        command: &str,
        cwd: Option<&str>,
        env: &HashMap<String, String>,
        timeout: Option<Duration>,
        mut on_chunk: F,
    ) -> Result<SessionExecResult, SessionExecFailure>
    where
        F: FnMut(&str) -> AppResult<()>,
    {
        validate_name(name).map_err(|e| SessionExecFailure::from_error(e, Duration::ZERO))?;

        // Serialize commands per pane; two send-keys calls must never interleave.
        let lock_key = format!(
            "{}@{}:{}:{}",
            self.context.user, self.context.cluster.host, self.context.cluster.port, name
        );
        let lock = SESSION_LOCKS.lock().entry(lock_key).or_default().clone();
        let _guard = lock.lock().await;
        let start = Instant::now();
        let limit = timeout.unwrap_or(Duration::from_secs(300));
        if limit.is_zero() {
            return Err(SessionExecFailure::from_error(
                AppError::InvalidInput("timeout_s must be greater than zero".into()),
                Duration::ZERO,
            ));
        }
        crate::util::validate_env(env)
            .map_err(|e| SessionExecFailure::from_error(e, Duration::ZERO))?;
        // Private files hold the exact output independently of terminal width/history.
        let dir = format!("/tmp/gwu-hpc-mcp-{}", Uuid::new_v4());
        let setup = format!(
            "command -v timeout >/dev/null && (umask 077; mkdir {})",
            escape_for_shell(&dir)
        );
        remote(&self.context, setup)
            .await
            .map_err(|e| SessionExecFailure::from_error(e, start.elapsed()))?;
        let mut statements = Vec::new();
        if let Some(path) = cwd {
            statements.push(format!("cd {}", escape_for_shell(path)));
        }
        for (key, value) in env {
            statements.push(format!("export {}={}", key, escape_for_shell(value)));
        }
        // Run in a child Bash so exit/clear and shell state cannot destroy tracking.
        statements.push(format!("eval {}", escape_for_shell(command)));
        let script = statements.join(" && ");
        let wrapper = format!(
            "timeout --signal=TERM --kill-after=2s {}s bash -lc {} >{dir}/output 2>&1; printf '%s\\n' $? >{dir}/status.tmp; mv {dir}/status.tmp {dir}/status",
            limit.as_secs(),
            escape_for_shell(&script)
        );
        let combined = format!("bash -c {}", escape_for_shell(&wrapper));
        let target = format!("={name}:0.0");
        let send = vec![
            "send-keys".into(),
            "-t".into(),
            target.clone(),
            "-l".into(),
            combined,
        ];
        let execute = async {
            run_tmux(&self.context, &send, Some(Duration::from_secs(10))).await?;
            run_tmux(&self.context, &["send-keys".into(), "-t".into(), target,
                "Enter".into()], Some(Duration::from_secs(10))).await?;
            loop {
                let poll = format!("if test -f {dir}/status; then cat {dir}/status {dir}/output; else printf 'RUNNING\\n'; fi");
                let result = remote(&self.context, poll).await?;
                let output = result.streams.get("stdout").cloned().unwrap_or_default();
                if let Some((code, data)) = parse_result(&output)? {
                    return Ok::<_, AppError>((code, data));
                }
                if start.elapsed() > limit + Duration::from_secs(20) {
                    return Err(AppError::Tmux(format!(
                        "No completion status received; inspect session {name} and {dir}")));
                }
                sleep(Duration::from_millis(400)).await;
            }
        }.await;
        // If SSH fails, keep files for diagnosis rather than deleting a running job's output.
        let (exit_code, bytes) =
            execute.map_err(|e| SessionExecFailure::from_error(e, start.elapsed()))?;
        let _ = remote(
            &self.context,
            format!("rm -f {dir}/output {dir}/status; rmdir {dir}"),
        )
        .await;
        let output = String::from_utf8_lossy(&bytes).into_owned();
        on_chunk(&output).map_err(|e| SessionExecFailure::from_error(e, start.elapsed()))?;
        if exit_code == 124 || exit_code == 137 {
            return Err(SessionExecFailure {
                error: AppError::SshTimeout(limit),
                duration: start.elapsed(),
                output,
            });
        }
        Ok(SessionExecResult {
            exit_code,
            duration: start.elapsed(),
            output,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SessionExecResult {
    pub exit_code: i32,
    pub duration: Duration,
    pub output: String,
}

#[derive(Debug)]
pub struct SessionExecFailure {
    pub error: AppError,
    pub duration: Duration,
    pub output: String,
}

impl SessionExecFailure {
    fn from_error(error: AppError, duration: Duration) -> Self {
        Self {
            error,
            duration,
            output: String::new(),
        }
    }
}

fn validate_name(name: &str) -> AppResult<()> {
    if name.is_empty() {
        return Err(AppError::InvalidInput(
            "Session name cannot be empty".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AppError::InvalidInput(
            "Session name must be alphanumeric with '_' or '-'".to_string(),
        ));
    }
    Ok(())
}

async fn run_tmux(
    context: &SshContext,
    args: &[String],
    timeout: Option<Duration>,
) -> AppResult<ExecResult> {
    let command = build_tmux_command(context.tmux_binary(), args);
    let req = ExecRequest {
        command,
        cwd: None,
        env: HashMap::new(),
        pty: false,
        login_shell: false,
        timeout,
    };
    match ssh::exec(context, req, |_, _| {}).await {
        Ok(result) => {
            if result.exit_code == 0 {
                Ok(result)
            } else {
                Err(AppError::Tmux(format!(
                    "tmux exited with code {}",
                    result.exit_code
                )))
            }
        }
        Err(failure) => Err(map_exec_failure("tmux command", failure)),
    }
}

// One command at a time per session in this server process.
static SESSION_LOCKS: once_cell::sync::Lazy<
    parking_lot::Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(HashMap::new()));

async fn remote(context: &SshContext, command: String) -> AppResult<ExecResult> {
    let req = ExecRequest {
        command,
        cwd: None,
        env: HashMap::new(),
        pty: false,
        login_shell: false,
        timeout: Some(Duration::from_secs(10)),
    };
    match ssh::exec(context, req, |_, _| {}).await {
        Ok(result) if result.exit_code == 0 => Ok(result),
        Ok(result) => Err(AppError::Tmux(format!(
            "Remote helper failed: {}",
            result.tail
        ))),
        Err(failure) => Err(map_exec_failure("session helper", failure)),
    }
}

fn parse_result(bytes: &[u8]) -> AppResult<Option<(i32, Vec<u8>)>> {
    let Some(end) = bytes.iter().position(|b| *b == b'\n') else {
        return Err(AppError::Tmux("Incomplete session status".into()));
    };
    if &bytes[..end] == b"RUNNING" {
        return Ok(None);
    }
    let code = std::str::from_utf8(&bytes[..end])
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .ok_or_else(|| AppError::Tmux("Invalid session status".into()))?;
    Ok(Some((code, bytes[end + 1..].to_vec())))
}

#[cfg(test)]
mod tests {
    use super::parse_result;
    #[test]
    fn output_is_independent_of_scrolling_unicode_and_terminal_codes() {
        let output = format!("{}\n\\x1b[2J中文🦀\n", "line\n".repeat(10000));
        let wire = format!("7\n{output}");
        let (code, bytes) = parse_result(wire.as_bytes()).unwrap().unwrap();
        assert_eq!(code, 7);
        assert_eq!(bytes, output.as_bytes());
        assert!(parse_result(b"RUNNING\n").unwrap().is_none());
        assert!(parse_result(b"bad\n").is_err());
    }
}

fn build_tmux_command(binary: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(escape_for_shell(binary));
    for arg in args {
        parts.push(escape_for_shell(arg));
    }
    parts.join(" ")
}

fn map_exec_failure(op: &str, failure: ExecFailure) -> AppError {
    match failure.error {
        AppError::SshTimeout(duration) => AppError::Tmux(format!(
            "{op} timed out after {}",
            format_duration(duration)
        )),
        other => AppError::Tmux(format!("{op} failed: {other}")),
    }
}
