//! Runs the real `ghm` binary against a local SSH server that accepts one key,
//! so `ghm up` needs no prompts. `up` must leave a detached background
//! connection that outlives it, and `status`, `test`, `serve-stdio` and `down`
//! must all reuse it. On Windows this covers the detached process spawn and
//! the named pipe between separate processes.
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const GHM: &str = env!("CARGO_BIN_EXE_ghm");
const HOSTNAME: &str = "fake-node";

#[derive(Default)]
struct Counters {
    logins: AtomicUsize,
    commands: AtomicUsize,
}

struct Conn {
    client_key: PublicKey,
    counters: Arc<Counters>,
    channels: std::collections::HashMap<ChannelId, Channel<Msg>>,
}

impl server::Handler for Conn {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        if key.key_data() == self.client_key.key_data() {
            self.counters.logins.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
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

    /// Every command "succeeds" and prints the host name, so the test needs
    /// no shell on the machine running it.
    async fn exec_request(
        &mut self,
        id: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(channel) = self.channels.remove(&id) else {
            return session.channel_failure(id);
        };
        self.counters.commands.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let _ = channel.data(format!("{HOSTNAME}\n").as_bytes()).await;
            let _ = channel.exit_status(0).await;
            let _ = channel.eof().await;
            let _ = channel.close().await;
        });
        session.channel_success(id)
    }
}

async fn start_server(client_key: PublicKey) -> (u16, PublicKey, Arc<Counters>) {
    let host = PrivateKey::from(Ed25519Keypair::from_seed(&[11; 32]));
    let host_public = host.public_key().clone();
    let config = Arc::new(server::Config {
        keys: vec![host],
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let counters = Arc::new(Counters::default());
    let shared = counters.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let handler = Conn {
                client_key: client_key.clone(),
                counters: shared.clone(),
                channels: Default::default(),
            };
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, socket, handler).await {
                    let _ = session.await;
                }
            });
        }
    });
    (port, host_public, counters)
}

/// A home directory holding the config, key and known_hosts. Dropping it
/// stops any connection the test left running, then deletes it.
struct Home {
    dir: PathBuf,
    config: PathBuf,
}

impl Home {
    fn new(port: u16, host_key: &PublicKey, client_key: &PrivateKey) -> Self {
        // Short: Unix socket paths are limited to ~104 bytes on macOS.
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = base.join(format!("ghm-e2e-{}", &id[..8]));
        std::fs::create_dir_all(&dir).unwrap();

        let key_path = dir.join("id_ed25519");
        std::fs::write(
            &key_path,
            client_key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        let known_hosts = dir.join("known_hosts");
        std::fs::write(
            &known_hosts,
            format!("[127.0.0.1]:{port} {}\n", host_key.to_openssh().unwrap()),
        )
        .unwrap();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"
version = 1

[defaults]
default_cluster = "test"
pty = false
login_shell = false

[[clusters]]
id = "test"
display_name = "Test cluster"
host = "127.0.0.1"
port = {port}

[clusters.ssh]
backend = "builtin"
control_path = '{cm}'
known_hosts_file = '{known_hosts}'
identity_files = ['{key}']
"#,
                cm = dir.join("cm-%r@%h_%p").display(),
                known_hosts = known_hosts.display(),
                key = key_path.display(),
            ),
        )
        .unwrap();
        Self { dir, config }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(GHM);
        command
            .arg("--config")
            .arg(&self.config)
            .arg("--user")
            .arg("alice")
            .args(args)
            .env("RUST_BACKTRACE", "1")
            .env("HOME", &self.dir)
            .env("USERPROFILE", &self.dir)
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("GWU_HPC_MCP_USER")
            .stdin(Stdio::null());
        command
    }

    /// Runs `ghm` without blocking the runtime, so the SSH server keeps serving.
    async fn run(&self, args: &[&str]) -> Output {
        let output = tokio::process::Command::from(self.command(args))
            .output()
            .await
            .unwrap();
        eprintln!(
            "$ ghm {} ({:?})\n{}{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join(".gwu-hpc-mcp").join("broker-test.log"))
            .unwrap_or_default()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["down"]).output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Minimal MCP client over `ghm serve-stdio`.
struct Mcp {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
}

impl Mcp {
    async fn start(home: &Home) -> Self {
        let mut child = tokio::process::Command::from(home.command(&["serve-stdio"]))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut mcp = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        mcp.call(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "background-up-test", "version": "1"}
            }),
        )
        .await;
        mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        mcp
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .unwrap()
                .expect("server exited");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                assert!(message.get("error").is_none(), "{message}");
                return message["result"].clone();
            }
        }
    }

    async fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self
            .call("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        assert_ne!(result["isError"], true, "{result}");
        result["structuredContent"].clone()
    }

    async fn close(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
    }
}

async fn wait_for_status(home: &Home, expected: &str) {
    for _ in 0..100 {
        if stdout(&home.run(&["status"]).await).contains(expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "status never became {expected:?}; broker log:\n{}",
        home.log()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn up_leaves_a_background_connection_that_other_processes_use() {
    let client_key = PrivateKey::from(Ed25519Keypair::from_seed(&[12; 32]));
    let (port, host_key, counters) = start_server(client_key.public_key().clone()).await;
    let home = Home::new(port, &host_key, &client_key);

    assert!(stdout(&home.run(&["status"]).await).contains("Status: missing"));

    // `up` must return once authenticated, leaving the broker running.
    let up = tokio::time::timeout(Duration::from_secs(60), home.run(&["up"]))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "`ghm up` did not return (a background process holding its \
                 output open also causes this); broker log:\n{}",
                home.log()
            )
        });
    assert!(
        up.status.success(),
        "`ghm up` exited with {:?}; broker log:\n{}",
        up.status,
        home.log()
    );
    let status = home.run(&["status"]).await;
    assert!(
        stdout(&status).contains("Status: running"),
        "broker log:\n{}",
        home.log()
    );

    let again = home.run(&["up"]).await;
    assert!(again.status.success());
    assert!(
        stdout(&again).contains("Already connected"),
        "{}",
        stdout(&again)
    );

    let test = home.run(&["test"]).await;
    assert!(test.status.success(), "broker log:\n{}", home.log());
    assert!(stdout(&test).contains(HOSTNAME), "{}", stdout(&test));

    // An MCP client in another process reuses the same connection.
    let mut mcp = Mcp::start(&home).await;
    let health = mcp.tool("health_check", json!({"cluster": "test"})).await;
    assert_eq!(health["master_state"], "Running", "{health}");
    let exec = mcp
        .tool(
            "shell_exec",
            json!({"cluster": "test", "command": "hostname", "timeout_s": 10}),
        )
        .await;
    assert_eq!(exec["ok"], true, "{exec}");
    assert_eq!(exec["streams"]["stdout"], format!("{HOSTNAME}\n"), "{exec}");
    mcp.close().await;

    assert_eq!(
        counters.logins.load(Ordering::SeqCst),
        1,
        "one login, reused"
    );
    assert!(counters.commands.load(Ordering::SeqCst) >= 2);

    assert!(home.run(&["down"]).await.status.success());
    wait_for_status(&home, "Status: missing").await;
}
