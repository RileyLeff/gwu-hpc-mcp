use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use uuid::Uuid;

use crate::errors::{AppError, AppResult};
use crate::ssh::{self, ExecFailure, ExecRequest, ExecResult, SshContext};
use crate::util::{TailBuffer, escape_for_shell, format_duration};

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

        let mut pre_command = String::new();
        if let Some(path) = cwd {
            pre_command.push_str(&format!("cd {} && ", escape_for_shell(path)));
        }
        for (key, value) in env {
            pre_command.push_str(&format!("export {}={} && ", escape_for_shell(key), escape_for_shell(value)));
        }
        let sentinel_id = Uuid::new_v4().to_string();
        let sentinel_marker = format!("MCP_DONE:{sentinel_id}:");
        let sentinel_line_prefix = format!("\n{sentinel_marker}");
        let combined_command = format!(
            "{pre_command}{command}; printf '\\n{marker}%s\\n' $?",
            marker = sentinel_marker
        );
        let target = format!("{name}:0.0");

        let args = vec![
            "send-keys".to_string(),
            "-t".to_string(),
            target.clone(),
            combined_command.clone(),
            "C-m".to_string(),
        ];

        if let Err(err) = run_tmux(&self.context, &args, Some(Duration::from_secs(10))).await {
            return Err(SessionExecFailure::from_error(err, Duration::ZERO));
        }

        let start = Instant::now();
        let mut last_buffer = String::new();
        let mut final_buffer = String::new();
        let mut tail = TailBuffer::new(4096);
        #[allow(unused_assignments)]
        let mut exit_code: Option<i32> = None;

        loop {
            if let Some(limit) = timeout
                && start.elapsed() > limit
            {
                let mut tail_buf = TailBuffer::new(4096);
                tail_buf.push(final_buffer.as_bytes());
                return Err(SessionExecFailure {
                    error: AppError::SshTimeout(limit),
                    duration: start.elapsed(),
                    output: final_buffer.clone(),
                    tail: tail_buf.to_lossy_string(),
                });
            }

            let capture = match capture_pane(&self.context, name).await {
                Ok(data) => data,
                Err(err) => {
                    return Err(SessionExecFailure::from_error(err, start.elapsed()));
                }
            };

            if capture.len() > last_buffer.len() {
                let new_chunk = &capture[last_buffer.len()..];
                let clean_chunk = if let Some(pos) = new_chunk.find(&sentinel_line_prefix) {
                    &new_chunk[..pos]
                } else {
                    new_chunk
                };
                if !clean_chunk.is_empty() {
                    tail.push(clean_chunk.as_bytes());
                    final_buffer.push_str(clean_chunk);
                    if let Err(err) = on_chunk(clean_chunk) {
                        return Err(SessionExecFailure::from_error(err, start.elapsed()));
                    }
                }
            }

            if let Some(pos) = capture.rfind(&sentinel_marker)
                && let Some(line_end) = capture[pos..].find('\n')
            {
                let line_end_abs = pos + line_end;
                let line = &capture[pos..line_end_abs];
                let code_str = line[sentinel_marker.len()..].trim();
                match code_str.parse::<i32>() {
                    Ok(code) => exit_code = Some(code),
                    Err(_) => {
                        return Err(SessionExecFailure {
                            error: AppError::Tmux(format!(
                                "Failed to parse tmux exit code from '{code_str}'"
                            )),
                            duration: start.elapsed(),
                            output: final_buffer.clone(),
                            tail: final_buffer.clone(),
                        });
                    }
                }
                break;
            }

            last_buffer = capture;
            sleep(Duration::from_millis(400)).await;
        }

        let duration = start.elapsed();
        let exit_code = match exit_code {
            Some(code) => code,
            None => {
                return Err(SessionExecFailure {
                    error: AppError::Tmux(
                        "Session terminated before sentinel was received".to_string(),
                    ),
                    duration,
                    output: final_buffer.clone(),
                    tail: final_buffer.clone(),
                });
            }
        };
        tail = TailBuffer::new(4096);
        tail.push(final_buffer.as_bytes());

        Ok(SessionExecResult {
            exit_code,
            duration,
            output: final_buffer.clone(),
            tail: tail.to_lossy_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct SessionExecResult {
    pub exit_code: i32,
    pub duration: Duration,
    pub output: String,
    pub tail: String,
}

#[derive(Debug)]
pub struct SessionExecFailure {
    pub error: AppError,
    pub duration: Duration,
    pub output: String,
    pub tail: String,
}

impl SessionExecFailure {
    fn from_error(error: AppError, duration: Duration) -> Self {
        Self {
            error,
            duration,
            output: String::new(),
            tail: String::new(),
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

async fn capture_pane(context: &SshContext, session: &str) -> AppResult<String> {
    let args = vec![
        "capture-pane".to_string(),
        "-p".to_string(),
        "-J".to_string(),
        "-t".to_string(),
        session.to_string(),
    ];
    let result = run_tmux(context, &args, Some(Duration::from_secs(10))).await?;
    let output = result
        .streams
        .get("stdout")
        .map(|buf| String::from_utf8_lossy(buf).into_owned())
        .unwrap_or_default();
    Ok(output)
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
