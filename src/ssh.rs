use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::{
    io::AsyncReadExt, process::Command, select, sync::mpsc, time, time::Instant as TokioInstant,
};
use tracing::warn;

use crate::{
    config::{Cluster, ClusterSsh},
    errors::{AppError, AppResult},
    util::{TailBuffer, escape_for_shell, expand_path, join_user_host},
};

#[derive(Debug, Clone)]
pub struct SshContext {
    pub cluster: Arc<Cluster>,
    pub user: String,
}

impl SshContext {
    pub fn new(cluster: Arc<Cluster>, user: String) -> Self {
        Self { cluster, user }
    }

    pub fn control_path(&self) -> AppResult<PathBuf> {
        render_control_path(
            &self.cluster.ssh,
            &self.user,
            &self.cluster.host,
            self.cluster.port,
        )
    }

    pub fn ssh_binary(&self) -> &str {
        &self.cluster.ssh.ssh_binary
    }

    pub fn tmux_binary(&self) -> &str {
        &self.cluster.ssh.tmux_binary
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterState {
    Missing,
    Running,
    Stale,
}

pub async fn check_master(context: &SshContext) -> AppResult<MasterState> {
    let control_path = context.control_path()?;
    if !control_path.exists() {
        return Ok(MasterState::Missing);
    }
    let mut cmd = Command::new(context.ssh_binary());
    cmd.arg("-S")
        .arg(&control_path)
        .arg("-o")
        .arg("ControlMaster=no")
        .arg("-O")
        .arg("check")
        .arg("-p")
        .arg(context.cluster.port.to_string())
        .arg(join_user_host(&context.user, &context.cluster.host))
        .stdout(Stdio::null());
    let status = cmd
        .status()
        .await
        .map_err(|e| AppError::SshSpawn(format!("ssh check failed: {e}")))?;
    if status.success() {
        Ok(MasterState::Running)
    } else {
        Ok(MasterState::Stale)
    }
}

pub async fn shutdown_master(context: &SshContext) -> AppResult<()> {
    let control_path = context.control_path()?;
    if !control_path.exists() {
        return Ok(());
    }
    let mut cmd = Command::new(context.ssh_binary());
    cmd.arg("-S")
        .arg(&control_path)
        .arg("-o")
        .arg("ControlMaster=no")
        .arg("-O")
        .arg("exit")
        .arg("-p")
        .arg(context.cluster.port.to_string())
        .arg(join_user_host(&context.user, &context.cluster.host))
        .stdout(Stdio::null());
    let status = cmd
        .status()
        .await
        .map_err(|e| AppError::SshSpawn(format!("ssh -O exit failed: {e}")))?;
    if status.success() {
        if let Err(e) = fs::remove_file(&control_path) {
            warn!(
                ?control_path,
                "failed to remove ControlPath after exit: {e}"
            );
        }
        Ok(())
    } else {
        Err(AppError::SshSpawn(format!(
            "ssh -O exit returned status {status}"
        )))
    }
}

pub async fn spawn_master_interactive(context: &SshContext) -> AppResult<()> {
    let control_path = context.control_path()?;
    if let Some(parent) = control_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            AppError::Io(format!(
                "failed to create ControlPath directory {}: {e}",
                parent.display()
            ))
        })?;
        // Set directory permissions to 700 for safety if possible
        #[cfg(unix)]
        {
            let metadata = fs::metadata(parent).map_err(|e| {
                AppError::Io(format!(
                    "failed to stat ControlPath directory {}: {e}",
                    parent.display()
                ))
            })?;
            let perms = metadata.permissions();
            if perms.mode() & 0o777 != 0o700 {
                let mut new_perms = perms;
                new_perms.set_mode(0o700);
                let _ = fs::set_permissions(parent, new_perms);
            }
        }
    }

    let mut cmd = Command::new(context.ssh_binary());
    cmd.arg("-MN")
        .arg("-f")
        .arg("-o")
        .arg("ControlMaster=yes")
        .arg("-o")
        .arg(format!(
            "ControlPath={}",
            control_path.as_path().to_str().unwrap_or_default()
        ))
        .arg("-o")
        .arg(format!(
            "ServerAliveInterval={}",
            context.cluster.ssh.server_alive_interval
        ))
        .arg("-o")
        .arg(format!(
            "ServerAliveCountMax={}",
            context.cluster.ssh.server_alive_count_max
        ))
        .arg("-o")
        .arg("ControlPersist=yes")
        .arg("-p")
        .arg(context.cluster.port.to_string())
        .arg(join_user_host(&context.user, &context.cluster.host))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd
        .status()
        .await
        .map_err(|e| AppError::SshSpawn(format!("failed to start ControlMaster: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::SshSpawn(format!(
            "ControlMaster exited with status {status}"
        )))
    }
}

#[derive(Debug)]
pub struct ExecRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    pub pty: bool,
    pub login_shell: bool,
    pub timeout: Option<Duration>,
}

#[derive(Debug)]
pub struct ExecResult {
    pub exit_code: i32,
    pub duration: Duration,
    pub streams: HashMap<String, Vec<u8>>,
    pub tail: String,
}

#[derive(Debug)]
pub struct ExecFailure {
    pub error: AppError,
    pub duration: Duration,
    pub streams: HashMap<String, Vec<u8>>,
    pub tail: String,
}

pub async fn exec<F>(
    context: &SshContext,
    req: ExecRequest,
    mut on_chunk: F,
) -> Result<ExecResult, ExecFailure>
where
    F: FnMut(&str, &[u8]) + Send,
{
    match exec_inner(context, req, &mut on_chunk).await {
        Ok(result @ ExecResult { .. }) => Ok(result),
        Err(failure) => Err(failure),
    }
}

async fn exec_inner<F>(
    context: &SshContext,
    req: ExecRequest,
    on_chunk: &mut F,
) -> Result<ExecResult, ExecFailure>
where
    F: FnMut(&str, &[u8]) + Send,
{
    let control_path = match context.control_path() {
        Ok(path) => path,
        Err(e) => {
            return Err(ExecFailure {
                error: e,
                duration: Duration::from_secs(0),
                streams: HashMap::new(),
                tail: String::new(),
            });
        }
    };
    let mut ssh_cmd = Command::new(context.ssh_binary());
    ssh_cmd
        .arg("-S")
        .arg(&control_path)
        .arg("-o")
        .arg("ControlMaster=no")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-p")
        .arg(context.cluster.port.to_string());
    if req.pty {
        ssh_cmd.arg("-tt");
    } else {
        ssh_cmd.arg("-T");
    }
    ssh_cmd.arg(join_user_host(&context.user, &context.cluster.host));

    let remote_command = build_remote_command(&req);
    ssh_cmd.arg(remote_command);
    ssh_cmd.kill_on_drop(true);
    ssh_cmd.stdin(Stdio::null());
    ssh_cmd.stdout(Stdio::piped());
    ssh_cmd.stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = match ssh_cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Err(ExecFailure {
                error: AppError::SshSpawn(format!("failed to spawn ssh: {e}")),
                duration: Duration::from_secs(0),
                streams: HashMap::new(),
                tail: String::new(),
            });
        }
    };

    let (tx, mut rx) = mpsc::channel::<StreamMessage>(32);
    let mut handles = Vec::new();

    if let Some(stdout) = child.stdout.take() {
        let stream_name: &'static str = if req.pty { "pty" } else { "stdout" };
        handles.push(spawn_stream_pump(stream_name, stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        handles.push(spawn_stream_pump("stderr", stderr, tx.clone()));
    }
    drop(tx);

    let mut tail = TailBuffer::new(4096);
    let mut streams: HashMap<String, Vec<u8>> = HashMap::new();
    let mut status_opt = None;
    let mut status_tick = Box::pin(time::sleep(Duration::from_millis(100)));
    let mut timed_out = false;
    let timeout_value = req.timeout;
    let mut timeout_fut = timeout_value.map(|d| Box::pin(time::sleep(d)));

    let mut channel_open = true;
    loop {
        if let Some(sleep) = timeout_fut.as_mut() {
            select! {
                maybe_msg = rx.recv(), if channel_open => {
                    match maybe_msg {
                        Some(StreamMessage::Data { name, chunk }) => {
                            if !chunk.is_empty() {
                                streams
                                    .entry(name.to_string())
                                    .or_default()
                                    .extend_from_slice(&chunk);
                                on_chunk(name, &chunk);
                                tail.push(&chunk);
                            }
                        }
                        Some(StreamMessage::Error { name, error }) => {
                            warn!(%name, %error, "error while reading ssh stream");
                        }
                        None => {
                            channel_open = false;
                        }
                    }
                }
                _ = status_tick.as_mut(), if status_opt.is_none() => {
                    match child.try_wait() {
                        Ok(Some(status)) => status_opt = Some(status),
                        Ok(None) => {
                            status_tick
                                .as_mut()
                                .reset(TokioInstant::now() + Duration::from_millis(100));
                        }
                        Err(e) => {
                            for handle in handles {
                                let _ = handle.await;
                            }
                            let duration = start.elapsed();
                            return Err(ExecFailure {
                                error: AppError::SshSpawn(format!("ssh try_wait failed: {e}")),
                                duration,
                                streams,
                                tail: tail.to_lossy_string(),
                            });
                        }
                    }
                }
                _ = sleep.as_mut(), if !timed_out => {
                    timed_out = true;
                    timeout_fut = None;
                    if let Err(e) = child.start_kill() {
                        warn!(error = ?e, "failed to kill ssh process after timeout");
                    }
                }
            }
        } else {
            select! {
                maybe_msg = rx.recv(), if channel_open => {
                    match maybe_msg {
                        Some(StreamMessage::Data { name, chunk }) => {
                            if !chunk.is_empty() {
                                streams
                                    .entry(name.to_string())
                                    .or_default()
                                    .extend_from_slice(&chunk);
                                on_chunk(name, &chunk);
                                tail.push(&chunk);
                            }
                        }
                        Some(StreamMessage::Error { name, error }) => {
                            warn!(%name, %error, "error while reading ssh stream");
                        }
                        None => {
                            channel_open = false;
                        }
                    }
                }
                _ = status_tick.as_mut(), if status_opt.is_none() => {
                    match child.try_wait() {
                        Ok(Some(status)) => status_opt = Some(status),
                        Ok(None) => {
                            status_tick
                                .as_mut()
                                .reset(TokioInstant::now() + Duration::from_millis(100));
                        }
                        Err(e) => {
                            for handle in handles {
                                let _ = handle.await;
                            }
                            let duration = start.elapsed();
                            return Err(ExecFailure {
                                error: AppError::SshSpawn(format!("ssh try_wait failed: {e}")),
                                duration,
                                streams,
                                tail: tail.to_lossy_string(),
                            });
                        }
                    }
                }
            }
        }

        if !channel_open && status_opt.is_some() {
            break;
        }
    }

    for handle in handles {
        let _ = handle.await;
    }

    let duration = start.elapsed();
    if timed_out {
        return Err(ExecFailure {
            error: AppError::SshTimeout(timeout_value.unwrap_or(Duration::from_secs(0))),
            duration,
            streams,
            tail: tail.to_lossy_string(),
        });
    }

    let exit_status = status_opt.expect("status present");
    let code = exit_status.code().unwrap_or(255);

    Ok(ExecResult {
        exit_code: code,
        duration,
        streams,
        tail: tail.to_lossy_string(),
    })
}

fn build_remote_command(req: &ExecRequest) -> String {
    let mut statements = Vec::new();
    if let Some(cwd) = &req.cwd {
        statements.push(format!("cd {}", escape_for_shell(cwd)));
    }
    for (key, value) in &req.env {
        statements.push(format!("export {}={}", key, escape_for_shell(value)));
    }
    statements.push(format!("eval {}", escape_for_shell(&req.command)));
    let joined = statements.join(" && ");
    if req.login_shell {
        format!("bash -lc {}", escape_for_shell(&joined))
    } else {
        joined
    }
}

fn render_control_path(ssh: &ClusterSsh, user: &str, host: &str, port: u16) -> AppResult<PathBuf> {
    let replaced = ssh
        .control_path
        .replace("%r", user)
        .replace("%h", host)
        .replace("%p", &port.to_string());
    expand_path(&replaced)
}

enum StreamMessage {
    Data { name: &'static str, chunk: Vec<u8> },
    Error { name: &'static str, error: String },
}

fn spawn_stream_pump(
    name: &'static str,
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    sender: mpsc::Sender<StreamMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    if sender
                        .send(StreamMessage::Data { name, chunk })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    let _ = sender
                        .send(StreamMessage::Error {
                            name,
                            error: e.to_string(),
                        })
                        .await;
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_cwd_stops_entire_multistatement_command() {
        let req = ExecRequest {
            command: "printf first; printf MUST_NOT_RUN".into(),
            cwd: Some("/nonexistent-ghm-test-directory".into()),
            env: HashMap::new(),
            pty: false,
            login_shell: false,
            timeout: None,
        };
        let result = std::process::Command::new("bash")
            .arg("-c")
            .arg(build_remote_command(&req))
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
}
