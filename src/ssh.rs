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
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    select,
    sync::{mpsc, oneshot},
    time,
};
use tracing::warn;

use crate::{
    broker,
    config::{Cluster, ClusterSsh, SshBackend},
    errors::{AppError, AppResult},
    ipc,
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

    /// The backend in effect: `auto` is the built-in client on Windows.
    pub fn backend(&self) -> SshBackend {
        self.cluster.ssh.backend.resolve()
    }

    /// Where the shared cluster connection lives, for status and errors.
    pub fn connection_endpoint(&self) -> AppResult<String> {
        match self.backend() {
            SshBackend::Builtin => Ok(ipc::endpoint(self)?.to_string()),
            _ => Ok(self.control_path()?.display().to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterState {
    Missing,
    Running,
    Stale,
}

pub async fn check_master(context: &SshContext) -> AppResult<MasterState> {
    if context.backend() == SshBackend::Builtin {
        return broker::check(context).await;
    }
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
    if context.backend() == SshBackend::Builtin {
        return broker::shutdown(context).await;
    }
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
    match exec_inner(context, req, None, &mut on_chunk).await {
        Ok(result @ ExecResult { .. }) => Ok(result),
        Err(failure) => Err(failure),
    }
}

/// Internal data transport for the fixed, non-executing upload receiver.
/// Not exposed as arbitrary stdin on shell_exec or session_exec.
pub(crate) async fn exec_with_input(
    context: &SshContext,
    req: ExecRequest,
    input: Vec<u8>,
) -> Result<ExecResult, ExecFailure> {
    exec_inner(context, req, Some(input), &mut |_, _| {}).await
}

async fn exec_inner<F>(
    context: &SshContext,
    req: ExecRequest,
    input: Option<Vec<u8>>,
    on_chunk: &mut F,
) -> Result<ExecResult, ExecFailure>
where
    F: FnMut(&str, &[u8]) + Send,
{
    let start = Instant::now();
    let fail = |error| ExecFailure {
        error,
        duration: Duration::from_secs(0),
        streams: HashMap::new(),
        tail: String::new(),
    };
    let has_input = input.is_some();
    let started = match context.backend() {
        SshBackend::Builtin => start_builtin(context, &req, input).await,
        _ => start_openssh(context, &req, input),
    };
    let mut running = started.map_err(fail)?;

    let mut tail = TailBuffer::new(4096);
    let mut streams: HashMap<String, Vec<u8>> = HashMap::new();
    let mut exit_code = None;
    let mut input_failed = false;
    let mut failed = None;
    let mut timed_out = false;
    let deadline = req.timeout.map(|d| time::Instant::now() + d);
    loop {
        let event = select! {
            event = running.events.recv() => event,
            _ = sleep_until(deadline), if !timed_out => {
                timed_out = true;
                running.kill();
                continue;
            }
        };
        match event {
            Some(Event::Data(name, chunk)) => {
                if !chunk.is_empty() {
                    streams
                        .entry(name.to_string())
                        .or_default()
                        .extend_from_slice(&chunk);
                    on_chunk(name, &chunk);
                    tail.push(&chunk);
                }
            }
            Some(Event::Exit(code)) => exit_code = Some(code),
            Some(Event::InputFailed) => input_failed = true,
            Some(Event::Failed(error)) => failed = Some(error),
            None => break,
        }
    }

    let error = if timed_out {
        Some(AppError::SshTimeout(req.timeout.unwrap_or_default()))
    } else if let Some(error) = failed {
        Some(error)
    } else if input_failed && has_input {
        Some(AppError::Io(
            "upload stdin transfer failed; reconcile destination before retry".into(),
        ))
    } else if exit_code.is_none() {
        Some(AppError::SshSpawn(
            "lost the cluster connection before the command finished".into(),
        ))
    } else {
        None
    };
    let duration = start.elapsed();
    match error {
        Some(error) => Err(ExecFailure {
            error,
            duration,
            streams,
            tail: tail.to_lossy_string(),
        }),
        None => Ok(ExecResult {
            exit_code: exit_code.unwrap_or(255),
            duration,
            streams,
            tail: tail.to_lossy_string(),
        }),
    }
}

async fn sleep_until(deadline: Option<time::Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

enum Event {
    Data(&'static str, Vec<u8>),
    /// Final event when the command ran to completion (all output precedes it).
    Exit(i32),
    InputFailed,
    Failed(AppError),
}

/// A command started on some backend. Dropping it stops the command.
struct Running {
    events: mpsc::Receiver<Event>,
    kill: Option<oneshot::Sender<()>>,
}

impl Running {
    fn kill(&mut self) {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
    }
}

fn stdout_name(req: &ExecRequest) -> &'static str {
    if req.pty { "pty" } else { "stdout" }
}

fn start_openssh(
    context: &SshContext,
    req: &ExecRequest,
    input: Option<Vec<u8>>,
) -> AppResult<Running> {
    let control_path = context.control_path()?;
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
    ssh_cmd.arg(build_remote_command(req));
    ssh_cmd.kill_on_drop(true);
    ssh_cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    ssh_cmd.stdout(Stdio::piped());
    ssh_cmd.stderr(Stdio::piped());
    let mut child = ssh_cmd
        .spawn()
        .map_err(|e| AppError::SshSpawn(format!("failed to spawn ssh: {e}")))?;

    // Concurrent bounded input avoids blocking before the execution timeout.
    // Dropping stdin after the write signals EOF to the receiver.
    let input_task = input.map(|data| {
        let mut stdin = child.stdin.take().expect("piped upload stdin");
        tokio::spawn(async move { stdin.write_all(&data).await })
    });
    let (tx, events) = mpsc::channel::<Event>(32);
    let mut pumps = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        pumps.push(spawn_stream_pump(stdout_name(req), stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        pumps.push(spawn_stream_pump("stderr", stderr, tx.clone()));
    }
    let (kill, mut kill_rx) = oneshot::channel();
    tokio::spawn(async move {
        // A kill request, or the Running handle being dropped, stops ssh.
        let status = select! {
            status = child.wait() => status,
            _ = &mut kill_rx => {
                if let Err(e) = child.start_kill() {
                    warn!(error = ?e, "failed to kill ssh process");
                }
                child.wait().await
            }
        };
        for pump in pumps {
            let _ = pump.await;
        }
        if let Some(task) = input_task {
            // Child has exited (or been killed); its pipe cannot keep a writer alive.
            if !matches!(task.await, Ok(Ok(()))) {
                let _ = tx.send(Event::InputFailed).await;
            }
        }
        let _ = tx
            .send(match status {
                Ok(status) => Event::Exit(status.code().unwrap_or(255)),
                Err(e) => Event::Failed(AppError::SshSpawn(format!("ssh wait failed: {e}"))),
            })
            .await;
    });
    Ok(Running {
        events,
        kill: Some(kill),
    })
}

async fn start_builtin(
    context: &SshContext,
    req: &ExecRequest,
    input: Option<Vec<u8>>,
) -> AppResult<Running> {
    let endpoint = ipc::endpoint(context)?;
    let request = ipc::Request::Exec {
        command: build_remote_command(req),
        pty: req.pty,
    };
    let (mut reader, mut writer) = ipc::request(&endpoint, request).await?;
    let stdout = stdout_name(req);
    let (tx, events) = mpsc::channel::<Event>(32);
    let (kill, mut kill_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        // Stdin goes from its own task so output keeps flowing meanwhile.
        let mut input_task = tokio::spawn(async move {
            if let Some(data) = input {
                writer.stdin(&data).await?;
            }
            writer.stdin_eof().await?;
            // Keep the write half open: closing the connection stops the command.
            std::future::pending::<()>().await;
            std::io::Result::Ok(())
        });
        let mut input_failed = false;
        loop {
            let frame = select! {
                // Kill or drop: closing the connection makes the broker stop the command.
                _ = &mut kill_rx => break,
                done = &mut input_task, if !input_task.is_finished() => {
                    input_failed = !matches!(done, Ok(Ok(())));
                    continue;
                }
                frame = reader.frame() => frame,
            };
            let event = match frame {
                Ok(Some(ipc::Frame::Stdout(data))) => Event::Data(stdout, data.to_vec()),
                Ok(Some(ipc::Frame::Stderr(data))) => Event::Data("stderr", data.to_vec()),
                Ok(Some(ipc::Frame::Control(body))) => {
                    let event = match ipc::parse::<ipc::Reply>(&body) {
                        Ok(ipc::Reply::Exit { code, .. }) => {
                            if input_failed {
                                let _ = tx.send(Event::InputFailed).await;
                            }
                            // Like OpenSSH: no exit status (e.g. killed by a signal) is 255.
                            Event::Exit(code.map_or(255, |c| c as i32))
                        }
                        Ok(ipc::Reply::Error { message }) => {
                            Event::Failed(AppError::SshSpawn(message))
                        }
                        Ok(other) => Event::Failed(AppError::SshSpawn(format!(
                            "unexpected reply from the cluster connection: {other:?}"
                        ))),
                        Err(e) => Event::Failed(AppError::SshSpawn(format!(
                            "bad reply from the cluster connection: {e}"
                        ))),
                    };
                    let _ = tx.send(event).await;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            };
            if tx.send(event).await.is_err() {
                break;
            }
        }
        input_task.abort();
    });
    Ok(Running {
        events,
        kill: Some(kill),
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

fn spawn_stream_pump(
    name: &'static str,
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    sender: mpsc::Sender<Event>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if sender
                        .send(Event::Data(name, buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    warn!(%name, error = %e, "error while reading ssh stream");
                    break;
                }
            }
        }
    })
}

// Runs the remote command locally with bash.
#[cfg(all(test, unix))]
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
