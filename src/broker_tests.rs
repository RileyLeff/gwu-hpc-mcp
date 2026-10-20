//! End-to-end tests for the built-in SSH backend against an in-process SSH
//! server that asks for a password and a Duo-style second factor, like
//! Pegasus, and runs commands with a real local shell.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, ChannelMsg, MethodKind, MethodSet};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use crate::broker::{self, AuthVia, Prompter, Settings};
use crate::config::Cluster;
use crate::errors::AppError;
use crate::ipc::{self, PromptItem};
use crate::known_hosts;
use crate::ssh::{self, ExecRequest, MasterState, SshContext};

const PASSWORD: &str = "correct horse";
const PASSCODE: &str = "424242";

// ---------------------------------------------------------------------------
// Test SSH server

#[derive(Default)]
struct Counters {
    /// Keyboard-interactive starts; proves no credentials were sent.
    auth_starts: AtomicUsize,
    commands: AtomicUsize,
    killed: AtomicUsize,
}

struct TestServer {
    port: u16,
    host_key: PublicKey,
    counters: Arc<Counters>,
}

fn host_key(seed: u8) -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

async fn start_server(seed: u8) -> TestServer {
    let key = host_key(seed);
    let host_key = key.public_key().clone();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        methods: MethodSet::from(&[MethodKind::KeyboardInteractive][..]),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let counters = Arc::new(Counters::default());
    let shared = counters.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let handler = Conn {
                counters: shared.clone(),
                awaiting_passcode: false,
                channels: HashMap::new(),
                kills: HashMap::new(),
            };
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, socket, handler).await {
                    let _ = session.await;
                }
            });
        }
    });
    TestServer {
        port,
        host_key,
        counters,
    }
}

struct Conn {
    counters: Arc<Counters>,
    awaiting_passcode: bool,
    channels: HashMap<ChannelId, Channel<Msg>>,
    kills: HashMap<ChannelId, oneshot::Sender<()>>,
}

fn keyboard_interactive() -> Option<MethodSet> {
    Some(MethodSet::from(&[MethodKind::KeyboardInteractive][..]))
}

impl server::Handler for Conn {
    type Error = russh::Error;

    async fn authentication_banner(&mut self) -> Result<Option<String>, Self::Error> {
        Ok(Some("Authorized use only.\n".into()))
    }

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: keyboard_interactive(),
            partial_success: false,
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let reject = Auth::Reject {
            proceed_with_methods: keyboard_interactive(),
            partial_success: false,
        };
        let Some(response) = response else {
            self.counters.auth_starts.fetch_add(1, Ordering::SeqCst);
            self.awaiting_passcode = false;
            return Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Password: ".into(), false)].into(),
            });
        };
        let answers: Vec<String> = response
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect();
        if !self.awaiting_passcode {
            if answers != [PASSWORD] {
                return Ok(reject);
            }
            self.awaiting_passcode = true;
            return Ok(Auth::Partial {
                name: "Duo two-factor login".into(),
                instructions: "Enter a passcode or select an option.".into(),
                prompts: vec![("Passcode or option (1-2): ".into(), true)].into(),
            });
        }
        Ok(if answers == [PASSCODE] {
            Auth::Accept
        } else {
            reject
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _cols: u32,
        _rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(channel) = self.channels.remove(&id) else {
            return session.channel_failure(id);
        };
        let (kill, killed) = oneshot::channel();
        self.kills.insert(id, kill);
        self.counters.commands.fetch_add(1, Ordering::SeqCst);
        let command = String::from_utf8_lossy(data).into_owned();
        tokio::spawn(run_command(channel, command, killed, self.counters.clone()));
        session.channel_success(id)
    }

    async fn signal(
        &mut self,
        channel: ChannelId,
        _signal: russh::Sig,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(kill) = self.kills.remove(&channel) {
            let _ = kill.send(());
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(kill) = self.kills.remove(&channel) {
            let _ = kill.send(());
        }
        Ok(())
    }
}

fn shell() -> Option<PathBuf> {
    if cfg!(windows) {
        let git_bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        git_bash.exists().then_some(git_bash)
    } else {
        Some(PathBuf::from("sh"))
    }
}

async fn run_command(
    mut channel: Channel<Msg>,
    command: String,
    mut killed: oneshot::Receiver<()>,
    counters: Arc<Counters>,
) {
    // Pass the command in the environment: Git Bash parses its Windows command
    // line with Cygwin rules, which would mangle single-quoted arguments.
    let mut child = tokio::process::Command::new(shell().unwrap())
        .arg("-c")
        .arg(r#"eval "$GHM_TEST_COMMAND""#)
        .env("GHM_TEST_COMMAND", &command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut out = channel.make_writer();
    let mut err = channel.make_writer_ext(Some(1));
    let pumps = [
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stdout, &mut out).await;
        }),
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stderr, &mut err).await;
        }),
    ];
    let mut open = true;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.ok(),
            _ = &mut killed => {
                counters.killed.fetch_add(1, Ordering::SeqCst);
                let _ = child.start_kill();
                break child.wait().await.ok();
            }
            msg = channel.wait(), if open => match msg {
                Some(ChannelMsg::Data { data }) => {
                    if let Some(stdin) = stdin.as_mut() {
                        let _ = stdin.write_all(&data).await;
                    }
                }
                Some(ChannelMsg::Eof) => stdin = None,
                Some(_) => {}
                None => open = false,
            },
        }
    };
    for pump in pumps {
        let _ = pump.await;
    }
    if let Some(code) = status.and_then(|s| s.code()) {
        let _ = channel.exit_status(code as u32).await;
    }
    let _ = channel.eof().await;
    let _ = channel.close().await;
}

// ---------------------------------------------------------------------------
// Client side helpers

/// Answers prompts like a user would and records what was shown.
struct Script {
    password: String,
    passcode: String,
    confirm_host: Option<bool>,
    shown: Vec<String>,
}

impl Script {
    fn new(confirm_host: Option<bool>) -> Self {
        Self {
            password: PASSWORD.into(),
            passcode: PASSCODE.into(),
            confirm_host,
            shown: Vec::new(),
        }
    }

    fn transcript(&self) -> String {
        self.shown.join("\n")
    }
}

impl Prompter for Script {
    async fn info(&mut self, text: &str) -> crate::errors::AppResult<()> {
        self.shown.push(text.to_string());
        Ok(())
    }

    async fn prompt(
        &mut self,
        name: &str,
        instructions: &str,
        prompts: &[PromptItem],
    ) -> crate::errors::AppResult<Vec<String>> {
        self.shown.push(format!("{name}|{instructions}"));
        Ok(prompts
            .iter()
            .map(|p| {
                self.shown.push(p.text.clone());
                if p.text.starts_with("Password") {
                    assert!(!p.echo, "passwords must not echo");
                    self.password.clone()
                } else if p.text.starts_with("Passcode") {
                    self.passcode.clone()
                } else {
                    panic!("unexpected prompt {:?}", p.text)
                }
            })
            .collect())
    }

    async fn confirm(&mut self, message: &str) -> crate::errors::AppResult<bool> {
        self.shown.push(message.to_string());
        Ok(self
            .confirm_host
            .unwrap_or_else(|| panic!("unexpected host key confirmation: {message}")))
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        // Short: Unix socket paths are limited to ~104 bytes on macOS.
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = base.join(format!("ghm-{}", &id[..8]));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn settings(server: &TestServer, dir: &Path) -> Settings {
    Settings {
        host: "127.0.0.1".into(),
        port: server.port,
        user: "alice".into(),
        keepalive: Duration::from_secs(30),
        keepalive_max: 3,
        identity_files: vec![dir.join("no_such_key")],
        known_hosts: dir.join("known_hosts"),
    }
}

fn context(server: &TestServer, dir: &Path) -> SshContext {
    let dir = dir.display();
    let cluster: Cluster = toml::from_str(&format!(
        r#"
        id = "test"
        host = "127.0.0.1"
        port = {port}
        [ssh]
        backend = "builtin"
        control_path = '{dir}/cm-%r@%h_%p'
        known_hosts_file = '{dir}/known_hosts'
        identity_files = ['{dir}/no_such_key']
        "#,
        port = server.port
    ))
    .unwrap();
    SshContext::new(Arc::new(cluster), "alice".into())
}

fn request(command: &str, timeout: Option<Duration>) -> ExecRequest {
    ExecRequest {
        command: command.into(),
        cwd: None,
        env: HashMap::new(),
        pty: false,
        login_shell: false,
        timeout,
    }
}

fn text(streams: &HashMap<String, Vec<u8>>, name: &str) -> String {
    String::from_utf8_lossy(streams.get(name).map(Vec::as_slice).unwrap_or_default()).into_owned()
}

async fn with_timeout<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .expect("test timed out")
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn unknown_host_is_confirmed_before_any_credential_is_sent() {
    with_timeout(async {
        let server = start_server(1).await;
        let dir = TempDir::new();
        let settings = settings(&server, &dir.0);

        let mut declined = Script::new(Some(false));
        let err = broker::establish(&settings, &mut declined)
            .await
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("Host key verification failed"),
            "{err}"
        );
        assert!(
            declined
                .transcript()
                .contains(&known_hosts::fingerprint(&server.host_key))
        );
        assert_eq!(server.counters.auth_starts.load(Ordering::SeqCst), 0);
        assert!(!settings.known_hosts.exists());

        let mut accepted = Script::new(Some(true));
        let handle = broker::establish(&settings, &mut accepted).await.unwrap();
        let shown = accepted.transcript();
        assert!(shown.contains("Authorized use only."), "{shown}");
        assert!(shown.contains("Duo two-factor login"), "{shown}");
        assert!(shown.contains("Permanently added '[127.0.0.1]:"), "{shown}");
        drop(handle);

        // Now trusted: no confirmation (the script panics if asked).
        let mut trusted = Script::new(None);
        broker::establish(&settings, &mut trusted).await.unwrap();
    })
    .await
}

#[tokio::test]
async fn changed_host_key_is_refused_without_authenticating() {
    with_timeout(async {
        let server = start_server(2).await;
        let dir = TempDir::new();
        let settings = settings(&server, &dir.0);
        let impostor = host_key(3);
        known_hosts::learn(
            &settings.known_hosts,
            "127.0.0.1",
            server.port,
            impostor.public_key(),
        )
        .unwrap();
        let err = broker::establish(&settings, &mut Script::new(None))
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("HAS CHANGED"), "{err}");
        assert_eq!(server.counters.auth_starts.load(Ordering::SeqCst), 0);
    })
    .await
}

#[tokio::test]
async fn wrong_password_is_retried_then_refused() {
    with_timeout(async {
        let server = start_server(4).await;
        let dir = TempDir::new();
        let settings = settings(&server, &dir.0);
        let mut script = Script::new(Some(true));
        script.password = "wrong".into();
        let err = broker::establish(&settings, &mut script)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("Permission denied"), "{err}");
        assert_eq!(server.counters.auth_starts.load(Ordering::SeqCst), 3);
        assert!(!script.transcript().contains("Passcode"));
    })
    .await
}

#[tokio::test]
async fn broker_runs_commands_for_other_processes() {
    if shell().is_none() {
        eprintln!("skipping: no POSIX shell for the test server");
        return;
    }
    with_timeout(async {
        let server = start_server(5).await;
        let dir = TempDir::new();
        let context = context(&server, &dir.0);
        let endpoint = ipc::endpoint(&context).unwrap();
        assert_eq!(
            ssh::check_master(&context).await.unwrap(),
            MasterState::Missing
        );

        let broker_context = context.clone();
        let broker =
            tokio::spawn(async move { broker::run(&broker_context, AuthVia::Attach).await });
        broker::attach(&endpoint, &mut Script::new(Some(true)))
            .await
            .unwrap();
        assert_eq!(
            ssh::check_master(&context).await.unwrap(),
            MasterState::Running
        );
        let status = broker::status(&endpoint).await.unwrap();
        assert!(status.ready);
        assert_eq!(status.target, format!("alice@127.0.0.1:{}", server.port));

        // Output streams and exit codes.
        let result = ssh::exec(
            &context,
            request("echo out; echo err >&2; exit 3", None),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, 3);
        assert_eq!(text(&result.streams, "stdout"), "out\n");
        assert_eq!(text(&result.streams, "stderr"), "err\n");

        // Quoting survives the round trip; cwd and env are applied.
        let mut req = request(
            "printf '%s|%s' \"$GREETING\" \"$(basename \"$PWD\")\"",
            None,
        );
        req.env
            .insert("GREETING".into(), "it's \"quoted\" $HOME".into());
        req.cwd = Some("/".into());
        let result = ssh::exec(&context, req, |_, _| {}).await.unwrap();
        assert_eq!(text(&result.streams, "stdout"), "it's \"quoted\" $HOME|/");

        // Upload-sized stdin larger than the SSH window, with output flowing.
        let input: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let result = ssh::exec_with_input(&context, request("wc -c", None), input)
            .await
            .unwrap();
        assert_eq!(text(&result.streams, "stdout").trim(), "3000000");
        let result = ssh::exec_with_input(&context, request("cat", None), b"hello".to_vec())
            .await
            .unwrap();
        assert_eq!(text(&result.streams, "stdout"), "hello");

        // Many concurrent commands share the one authenticated connection.
        let runs = (0..12).map(|i| {
            let context = context.clone();
            async move {
                ssh::exec(&context, request(&format!("echo {i}"), None), |_, _| {})
                    .await
                    .unwrap()
            }
        });
        for (i, result) in futures::future::join_all(runs).await.iter().enumerate() {
            assert_eq!(text(&result.streams, "stdout"), format!("{i}\n"));
        }

        // A timeout stops the remote command and the connection stays usable.
        let started = std::time::Instant::now();
        let failure = ssh::exec(
            &context,
            request("echo started; sleep 30", Some(Duration::from_millis(500))),
            |_, _| {},
        )
        .await
        .unwrap_err();
        assert!(
            matches!(failure.error, AppError::SshTimeout(_)),
            "{}",
            failure.error
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(text(&failure.streams, "stdout"), "started\n");
        for _ in 0..100 {
            if server.counters.killed.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(server.counters.killed.load(Ordering::SeqCst), 1);

        // Dropping the call (MCP cancellation) also stops the command.
        let cancelled = tokio::time::timeout(
            Duration::from_millis(300),
            ssh::exec(&context, request("sleep 30", None), |_, _| {}),
        )
        .await;
        assert!(cancelled.is_err());
        for _ in 0..100 {
            if server.counters.killed.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(server.counters.killed.load(Ordering::SeqCst), 2);

        let result = ssh::exec(&context, request("echo still here", None), |_, _| {})
            .await
            .unwrap();
        assert_eq!(text(&result.streams, "stdout"), "still here\n");

        // A second `up` sees the running connection; `down` stops it.
        ssh::shutdown_master(&context).await.unwrap();
        broker.await.unwrap().unwrap();
        assert_eq!(
            ssh::check_master(&context).await.unwrap(),
            MasterState::Missing
        );
        assert_eq!(server.counters.auth_starts.load(Ordering::SeqCst), 1);
    })
    .await
}

#[tokio::test]
async fn failed_authentication_is_reported_to_the_attached_terminal() {
    with_timeout(async {
        let server = start_server(6).await;
        let dir = TempDir::new();
        let context = context(&server, &dir.0);
        let endpoint = ipc::endpoint(&context).unwrap();
        let broker_context = context.clone();
        let broker =
            tokio::spawn(async move { broker::run(&broker_context, AuthVia::Attach).await });
        let mut script = Script::new(Some(true));
        script.passcode = "000000".into();
        let err = broker::attach(&endpoint, &mut script).await.unwrap_err();
        assert!(err.to_string().contains("Permission denied"), "{err}");
        assert!(broker.await.unwrap().is_err());
        // The endpoint is released so `up` can be retried.
        assert_eq!(
            ssh::check_master(&context).await.unwrap(),
            MasterState::Missing
        );
    })
    .await
}

/// A local stand-in for Pegasus for trying `up` by hand:
/// `cargo test serve_fake_cluster -- --ignored --nocapture`, then point a
/// config at the printed port (password "correct horse", passcode 424242).
#[tokio::test]
#[ignore]
async fn serve_fake_cluster() {
    let server = start_server(7).await;
    println!("fake cluster listening on 127.0.0.1:{}", server.port);
    tokio::time::sleep(Duration::from_secs(600)).await;
}
