//! Built-in SSH backend: a local broker process holds one authenticated SSH
//! connection and runs commands for `ghm` processes over local IPC.
//!
//! This replaces OpenSSH ControlMaster where it is unavailable (native
//! Windows). `ghm up` starts the broker in the background and relays its
//! password/2FA prompts to the user's terminal, so credentials only ever pass
//! between the terminal, the broker, and the cluster.
use std::future::Future;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelMsg, MethodKind, MethodSet, Sig};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, mpsc, oneshot};
use tracing::{info, warn};

use crate::errors::{AppError, AppResult};
use crate::ipc::{
    self, Answer, ConnReader, ConnWriter, ConnectError, Endpoint, Frame, Hello, PromptItem, Reply,
    Request, StatusInfo,
};
use crate::known_hosts::{self, HostKeyStatus};
use crate::ssh::{MasterState, SshContext};
use crate::util::{expand_path, home_dir};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_KEY_ATTEMPTS: usize = 5;
const MAX_FAILED_PROMPTS: usize = 3;

// ---------------------------------------------------------------------------
// Settings

#[derive(Debug, Clone)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub keepalive: Duration,
    pub keepalive_max: usize,
    pub identity_files: Vec<PathBuf>,
    pub known_hosts: PathBuf,
}

impl Settings {
    pub fn from_context(context: &SshContext) -> AppResult<Self> {
        let ssh = &context.cluster.ssh;
        let identity_files = if ssh.identity_files.is_empty() {
            let dir = home_dir()?.join(".ssh");
            ["id_ed25519", "id_ecdsa", "id_rsa"]
                .iter()
                .map(|name| dir.join(name))
                .collect()
        } else {
            ssh.identity_files
                .iter()
                .map(|p| expand_path(p))
                .collect::<AppResult<_>>()?
        };
        let known_hosts = match &ssh.known_hosts_file {
            Some(path) => expand_path(path)?,
            None => home_dir()?.join(".ssh").join("known_hosts"),
        };
        Ok(Self {
            host: context.cluster.host.clone(),
            port: context.cluster.port,
            user: context.user.clone(),
            keepalive: Duration::from_secs(ssh.server_alive_interval.max(1)),
            keepalive_max: ssh.server_alive_count_max.max(1) as usize,
            identity_files,
            known_hosts,
        })
    }

    fn target(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }
}

// ---------------------------------------------------------------------------
// Prompts: a terminal in-process, or relayed from the `ghm up` terminal.

pub trait Prompter: Send {
    fn info(&mut self, text: &str) -> impl Future<Output = AppResult<()>> + Send;
    fn prompt(
        &mut self,
        name: &str,
        instructions: &str,
        prompts: &[PromptItem],
    ) -> impl Future<Output = AppResult<Vec<String>>> + Send;
    fn confirm(&mut self, message: &str) -> impl Future<Output = AppResult<bool>> + Send;
}

/// Reads answers from the controlling terminal; hidden input for secrets.
pub struct TerminalPrompter;

impl Prompter for TerminalPrompter {
    async fn info(&mut self, text: &str) -> AppResult<()> {
        eprintln!("{}", text.trim_end());
        Ok(())
    }

    async fn prompt(
        &mut self,
        name: &str,
        instructions: &str,
        prompts: &[PromptItem],
    ) -> AppResult<Vec<String>> {
        for header in [name, instructions] {
            if !header.trim().is_empty() {
                eprintln!("{}", header.trim_end());
            }
        }
        let prompts = prompts.to_vec();
        tokio::task::spawn_blocking(move || {
            prompts
                .iter()
                .map(|p| {
                    if p.echo {
                        eprint!("{}", p.text);
                        let _ = std::io::stderr().flush();
                        let mut line = String::new();
                        std::io::stdin().lock().read_line(&mut line)?;
                        Ok(line.trim_end_matches(['\r', '\n']).to_string())
                    } else {
                        rpassword::prompt_password(&p.text)
                    }
                })
                .collect::<std::io::Result<Vec<_>>>()
        })
        .await
        .map_err(|e| AppError::Io(format!("prompt task failed: {e}")))?
        .map_err(|e| AppError::Io(format!("cannot read from terminal: {e}")))
    }

    async fn confirm(&mut self, message: &str) -> AppResult<bool> {
        eprintln!("{}", message.trim_end());
        let answer = self
            .prompt(
                "",
                "",
                &[PromptItem {
                    text: "Are you sure you want to continue connecting (yes/no)? ".into(),
                    echo: true,
                }],
            )
            .await?;
        Ok(answer
            .first()
            .is_some_and(|a| a.trim().eq_ignore_ascii_case("yes")))
    }
}

/// Terminal state saved before prompting, so Ctrl-C at a hidden prompt never
/// leaves the user's terminal without echo.
mod tty {
    use std::fs::{File, OpenOptions};

    #[cfg(unix)]
    pub struct Saved(File, libc::termios);

    #[cfg(unix)]
    pub fn save() -> Option<Saved> {
        use std::os::fd::AsRawFd;
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        // SAFETY: termios is plain data and tcgetattr fully initializes it on success.
        let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
        (unsafe { libc::tcgetattr(tty.as_raw_fd(), &mut term) } == 0).then_some(Saved(tty, term))
    }

    #[cfg(unix)]
    impl Saved {
        pub fn restore(&self) {
            use std::os::fd::AsRawFd;
            // SAFETY: restores attributes previously read from the same descriptor.
            unsafe { libc::tcsetattr(self.0.as_raw_fd(), libc::TCSANOW, &self.1) };
        }
    }

    #[cfg(windows)]
    pub struct Saved(File, u32);

    #[cfg(windows)]
    pub fn save() -> Option<Saved> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Console::GetConsoleMode;
        let console = OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
            .ok()?;
        let mut mode = 0;
        // SAFETY: the handle is a console input handle owned by `console`.
        (unsafe { GetConsoleMode(console.as_raw_handle(), &mut mode) } != 0)
            .then_some(Saved(console, mode))
    }

    #[cfg(windows)]
    impl Saved {
        pub fn restore(&self) {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::Console::SetConsoleMode;
            // SAFETY: restores the mode previously read from the same handle.
            unsafe { SetConsoleMode(self.0.as_raw_handle(), self.1) };
        }
    }
}

/// Run an interactive step; Ctrl-C restores the terminal and exits like ssh.
async fn interruptible<T>(work: impl Future<Output = AppResult<T>>) -> AppResult<T> {
    let saved = tty::save();
    // Registered before any prompt runs, so the interrupt that rpassword
    // raises on Ctrl-C reaches us instead of killing the process mid-prompt.
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| AppError::Io(format!("cannot handle Ctrl-C: {e}")))?;
    #[cfg(windows)]
    let mut interrupt = tokio::signal::windows::ctrl_c()
        .map_err(|e| AppError::Io(format!("cannot handle Ctrl-C: {e}")))?;
    tokio::select! {
        result = work => result,
        _ = interrupt.recv() => {
            if let Some(saved) = &saved {
                saved.restore();
            }
            eprintln!("\nInterrupted.");
            // A prompt may still be blocked reading the terminal; don't wait for it.
            std::process::exit(130);
        }
    }
}

/// Broker side of an attached `ghm up` terminal.
struct RelayPrompter {
    reader: ConnReader,
    writer: ConnWriter,
}

impl RelayPrompter {
    async fn answer(&mut self) -> AppResult<Answer> {
        match self.reader.control::<Answer>().await {
            Ok(Some(answer)) => Ok(answer),
            Ok(None) => Err(AppError::Io(
                "the ghm up terminal disconnected during authentication".into(),
            )),
            Err(e) => Err(AppError::Io(format!("authentication relay failed: {e}"))),
        }
    }

    async fn send(&mut self, reply: &Reply) -> AppResult<()> {
        self.writer
            .control(reply)
            .await
            .map_err(|e| AppError::Io(format!("authentication relay failed: {e}")))
    }
}

impl Prompter for RelayPrompter {
    async fn info(&mut self, text: &str) -> AppResult<()> {
        self.send(&Reply::Info { text: text.into() }).await
    }

    async fn prompt(
        &mut self,
        name: &str,
        instructions: &str,
        prompts: &[PromptItem],
    ) -> AppResult<Vec<String>> {
        self.send(&Reply::Prompt {
            name: name.into(),
            instructions: instructions.into(),
            prompts: prompts.to_vec(),
        })
        .await?;
        match self.answer().await? {
            Answer::Responses { responses } if responses.len() == prompts.len() => Ok(responses),
            other => Err(AppError::Io(format!("unexpected prompt answer: {other:?}"))),
        }
    }

    async fn confirm(&mut self, message: &str) -> AppResult<bool> {
        self.send(&Reply::Confirm {
            message: message.into(),
        })
        .await?;
        match self.answer().await? {
            Answer::Confirm { accept } => Ok(accept),
            other => Err(AppError::Io(format!(
                "unexpected confirm answer: {other:?}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// SSH connection and authentication

enum HostCheck {
    Trusted,
    /// Accepted at the transport level only; the user confirms before any
    /// credential is sent, and the connection is dropped if they decline.
    Unknown(PublicKey),
    Rejected(String),
}

pub struct ClientHandler {
    settings: Arc<Settings>,
    host_check: Arc<Mutex<Option<HostCheck>>>,
    banners: mpsc::UnboundedSender<String>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let s = &self.settings;
        let (verdict, accept) = match server_key {
            PublicKeyOrCertificate::Certificate(_) => (
                HostCheck::Rejected(
                    "the server offered a host certificate, which the built-in client does not verify"
                        .into(),
                ),
                false,
            ),
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                match known_hosts::check(&s.known_hosts, &s.host, s.port, key) {
                    Ok(HostKeyStatus::Trusted) => (HostCheck::Trusted, true),
                    Ok(HostKeyStatus::Unknown) => (HostCheck::Unknown(key.clone()), true),
                    Ok(HostKeyStatus::Changed { path, line }) => (
                        HostCheck::Rejected(format!(
                            "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED for {}.\n\
                             Someone could be eavesdropping on you (man-in-the-middle attack), or the host key was legitimately replaced.\n\
                             The {} key presented has fingerprint {}.\n\
                             Offending key is at {}:{line}. Confirm the new fingerprint with GW research computing before removing that line.\n\
                             Host key verification failed.",
                            known_hosts::host_token(&s.host, s.port),
                            key.algorithm(),
                            known_hosts::fingerprint(key),
                            path.display(),
                        )),
                        false,
                    ),
                    Ok(HostKeyStatus::Revoked { path, line }) => (
                        HostCheck::Rejected(format!(
                            "The host key for {} is marked as revoked ({}:{line}). Host key verification failed.",
                            s.host,
                            path.display()
                        )),
                        false,
                    ),
                    Err(e) => (
                        HostCheck::Rejected(format!(
                            "cannot read {}: {e}. Host key verification failed.",
                            s.known_hosts.display()
                        )),
                        false,
                    ),
                }
            }
        };
        *self.host_check.lock().unwrap() = Some(verdict);
        Ok(accept)
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let _ = self.banners.send(banner.to_string());
        Ok(())
    }
}

pub type SshHandle = Handle<ClientHandler>;

enum Outcome {
    Success,
    Failure { remaining: MethodSet, partial: bool },
}

impl From<russh::client::AuthResult> for Outcome {
    fn from(result: russh::client::AuthResult) -> Self {
        match result {
            russh::client::AuthResult::Success => Outcome::Success,
            russh::client::AuthResult::Failure {
                remaining_methods,
                partial_success,
            } => Outcome::Failure {
                remaining: remaining_methods,
                partial: partial_success,
            },
        }
    }
}

fn ssh_err(context: &str) -> impl Fn(russh::Error) -> AppError + '_ {
    move |e| AppError::SshSpawn(format!("{context}: {e}"))
}

/// Connect, verify the host key, and authenticate, asking `prompter` for
/// anything the server or the user needs to decide.
pub async fn establish<P: Prompter>(settings: &Settings, prompter: &mut P) -> AppResult<SshHandle> {
    let settings = Arc::new(settings.clone());
    let host_check = Arc::new(Mutex::new(None));
    let (banner_tx, mut banners) = mpsc::unbounded_channel();
    let handler = ClientHandler {
        settings: settings.clone(),
        host_check: host_check.clone(),
        banners: banner_tx,
    };
    let config = Arc::new(client::Config {
        keepalive_interval: Some(settings.keepalive),
        keepalive_max: settings.keepalive_max,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    });
    prompter
        .info(&format!("Connecting to {} ...", settings.target()))
        .await?;
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(config, (settings.host.as_str(), settings.port), handler),
    )
    .await;
    let rejected = || match host_check.lock().unwrap().take() {
        Some(HostCheck::Rejected(message)) => Some(message),
        _ => None,
    };
    let mut handle = match connected {
        Err(_) => {
            return Err(AppError::SshSpawn(format!(
                "timed out connecting to {}:{}. Off campus, check that GW VPN is connected.",
                settings.host, settings.port
            )));
        }
        Ok(Err(e)) => {
            return Err(AppError::SshSpawn(rejected().unwrap_or_else(|| {
                format!(
                    "cannot connect to {}:{}: {e}. Off campus, check that GW VPN is connected.",
                    settings.host, settings.port
                )
            })));
        }
        Ok(Ok(handle)) => handle,
    };

    let check = host_check.lock().unwrap().take();
    match check {
        Some(HostCheck::Trusted) => {}
        Some(HostCheck::Unknown(key)) => {
            let token = known_hosts::host_token(&settings.host, settings.port);
            let message = format!(
                "The authenticity of host '{token}' can't be established.\n\
                 {} key fingerprint is {}.\n\
                 Compare it with the fingerprint published by GW research computing before answering.",
                key.algorithm(),
                known_hosts::fingerprint(&key)
            );
            if !prompter.confirm(&message).await? {
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "", "")
                    .await;
                return Err(AppError::SshSpawn("Host key verification failed.".into()));
            }
            match known_hosts::learn(&settings.known_hosts, &settings.host, settings.port, &key) {
                Ok(()) => {
                    prompter
                        .info(&format!(
                            "Permanently added '{token}' to {}.",
                            settings.known_hosts.display()
                        ))
                        .await?
                }
                Err(e) => {
                    prompter
                        .info(&format!(
                            "Warning: could not record the host key in {}: {e}",
                            settings.known_hosts.display()
                        ))
                        .await?
                }
            }
        }
        Some(HostCheck::Rejected(message)) => return Err(AppError::SshSpawn(message)),
        None => return Err(AppError::SshSpawn("server host key was not checked".into())),
    }

    match authenticate(&mut handle, &settings, prompter, &mut banners).await {
        Ok(()) => Ok(handle),
        Err(e) => {
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await;
            Err(e)
        }
    }
}

async fn flush_banners<P: Prompter>(
    banners: &mut mpsc::UnboundedReceiver<String>,
    prompter: &mut P,
) -> AppResult<()> {
    while let Ok(banner) = banners.try_recv() {
        prompter.info(&banner).await?;
    }
    Ok(())
}

fn method_names(methods: &MethodSet) -> String {
    methods
        .iter()
        .map(<&str>::from)
        .collect::<Vec<_>>()
        .join(",")
}

async fn authenticate<P: Prompter>(
    handle: &mut SshHandle,
    settings: &Settings,
    prompter: &mut P,
    banners: &mut mpsc::UnboundedReceiver<String>,
) -> AppResult<()> {
    let user = settings.user.clone();
    let mut methods = match Outcome::from(
        handle
            .authenticate_none(&user)
            .await
            .map_err(ssh_err("authentication"))?,
    ) {
        Outcome::Success => return Ok(()),
        Outcome::Failure { remaining, .. } => remaining,
    };
    let mut tried_keys = false;
    let mut interactive_failures = 0;
    let mut password_failures = 0;
    // Bounded: every iteration consumes an attempt or a method.
    for _ in 0..16 {
        flush_banners(banners, prompter).await?;
        let outcome = if methods.contains(&MethodKind::PublicKey) && !tried_keys {
            tried_keys = true;
            match try_public_keys(handle, settings, prompter).await? {
                Some(outcome) => outcome,
                None => continue,
            }
        } else if methods.contains(&MethodKind::KeyboardInteractive)
            && interactive_failures < MAX_FAILED_PROMPTS
        {
            let outcome = keyboard_interactive(handle, &user, prompter, banners).await?;
            if matches!(outcome, Outcome::Failure { partial: false, .. }) {
                interactive_failures += 1;
            }
            outcome
        } else if methods.contains(&MethodKind::Password) && password_failures < MAX_FAILED_PROMPTS
        {
            let answers = prompter
                .prompt(
                    "",
                    "",
                    &[PromptItem {
                        text: format!("{user}@{}'s password: ", settings.host),
                        echo: false,
                    }],
                )
                .await?;
            let password = answers.into_iter().next().unwrap_or_default();
            let outcome: Outcome = handle
                .authenticate_password(&user, password)
                .await
                .map_err(ssh_err("password authentication"))?
                .into();
            if matches!(outcome, Outcome::Failure { partial: false, .. }) {
                password_failures += 1;
            }
            outcome
        } else {
            break;
        };
        match outcome {
            Outcome::Success => {
                flush_banners(banners, prompter).await?;
                return Ok(());
            }
            Outcome::Failure { remaining, partial } => {
                if partial {
                    prompter
                        .info("Authenticated with partial success; another factor is required.")
                        .await?;
                } else {
                    prompter
                        .info("Permission denied, please try again.")
                        .await?;
                }
                methods = remaining;
            }
        }
    }
    flush_banners(banners, prompter).await?;
    Err(AppError::SshSpawn(format!(
        "Permission denied ({}).",
        method_names(&methods)
    )))
}

async fn keyboard_interactive<P: Prompter>(
    handle: &mut SshHandle,
    user: &str,
    prompter: &mut P,
    banners: &mut mpsc::UnboundedReceiver<String>,
) -> AppResult<Outcome> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(user, None)
        .await
        .map_err(ssh_err("keyboard-interactive authentication"))?;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(Outcome::Success),
            KeyboardInteractiveAuthResponse::Failure {
                remaining_methods,
                partial_success,
            } => {
                return Ok(Outcome::Failure {
                    remaining: remaining_methods,
                    partial: partial_success,
                });
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                flush_banners(banners, prompter).await?;
                let items: Vec<PromptItem> = prompts
                    .into_iter()
                    .map(|p| PromptItem {
                        text: p.prompt,
                        echo: p.echo,
                    })
                    .collect();
                let answers = if items.is_empty() {
                    for text in [&name, &instructions] {
                        if !text.trim().is_empty() {
                            prompter.info(text).await?;
                        }
                    }
                    Vec::new()
                } else {
                    prompter.prompt(&name, &instructions, &items).await?
                };
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(ssh_err("keyboard-interactive authentication"))?;
            }
        }
    }
}

async fn rsa_hash(handle: &SshHandle) -> Option<russh::keys::HashAlg> {
    match handle.best_supported_rsa_hash().await {
        Ok(Some(hash)) => hash,
        // No server-sig-algs: modern servers still accept rsa-sha2-256.
        _ => Some(russh::keys::HashAlg::Sha256),
    }
}

/// Try agent keys, then identity files. Returns None if no key was accepted.
async fn try_public_keys<P: Prompter>(
    handle: &mut SshHandle,
    settings: &Settings,
    prompter: &mut P,
) -> AppResult<Option<Outcome>> {
    let mut attempts = 0;
    if let Some(outcome) = agent::try_agent(handle, &settings.user, &mut attempts).await {
        return Ok(Some(outcome));
    }
    for path in &settings.identity_files {
        if attempts >= MAX_KEY_ATTEMPTS {
            break;
        }
        if !path.exists() {
            continue;
        }
        let Some(key) = load_identity(path, prompter).await? else {
            continue;
        };
        attempts += 1;
        let hash = if key.algorithm().is_rsa() {
            rsa_hash(handle).await
        } else {
            None
        };
        let outcome: Outcome = handle
            .authenticate_publickey(
                &settings.user,
                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .map_err(ssh_err("public key authentication"))?
            .into();
        if !matches!(outcome, Outcome::Failure { partial: false, .. }) {
            return Ok(Some(outcome));
        }
    }
    Ok(None)
}

async fn load_identity<P: Prompter>(
    path: &Path,
    prompter: &mut P,
) -> AppResult<Option<russh::keys::PrivateKey>> {
    match russh::keys::load_secret_key(path, None) {
        Ok(key) => Ok(Some(key)),
        Err(russh::keys::Error::KeyIsEncrypted) => {
            let answers = prompter
                .prompt(
                    "",
                    "",
                    &[PromptItem {
                        text: format!(
                            "Enter passphrase for key '{}' (empty to skip): ",
                            path.display()
                        ),
                        echo: false,
                    }],
                )
                .await?;
            let passphrase = answers.into_iter().next().unwrap_or_default();
            if passphrase.is_empty() {
                return Ok(None);
            }
            match russh::keys::load_secret_key(path, Some(&passphrase)) {
                Ok(key) => Ok(Some(key)),
                Err(e) => {
                    prompter
                        .info(&format!("Skipping key {}: {e}", path.display()))
                        .await?;
                    Ok(None)
                }
            }
        }
        Err(e) => {
            warn!("skipping identity {}: {e}", path.display());
            Ok(None)
        }
    }
}

mod agent {
    use super::*;
    use russh::keys::agent::AgentIdentity;
    use russh::keys::agent::client::AgentClient;
    use tokio::io::{AsyncRead, AsyncWrite};

    pub async fn try_agent(
        handle: &mut SshHandle,
        user: &str,
        attempts: &mut usize,
    ) -> Option<Outcome> {
        #[cfg(unix)]
        {
            let agent = AgentClient::connect_env().await.ok()?;
            try_with(handle, user, agent, attempts).await
        }
        #[cfg(windows)]
        {
            let pipe = std::env::var("SSH_AUTH_SOCK")
                .unwrap_or_else(|_| r"\\.\pipe\openssh-ssh-agent".to_string());
            if let Ok(agent) = AgentClient::connect_named_pipe(&pipe).await
                && let Some(outcome) = try_with(handle, user, agent, attempts).await
            {
                return Some(outcome);
            }
            let agent = AgentClient::connect_pageant().await.ok()?;
            try_with(handle, user, agent, attempts).await
        }
    }

    async fn try_with<S: AsyncRead + AsyncWrite + Unpin + Send>(
        handle: &mut SshHandle,
        user: &str,
        mut agent: AgentClient<S>,
        attempts: &mut usize,
    ) -> Option<Outcome> {
        let identities = agent.request_identities().await.ok()?;
        for identity in identities {
            if *attempts >= MAX_KEY_ATTEMPTS {
                break;
            }
            let AgentIdentity::PublicKey { key, .. } = identity else {
                continue;
            };
            *attempts += 1;
            let hash = if key.algorithm().is_rsa() {
                rsa_hash(handle).await
            } else {
                None
            };
            match handle
                .authenticate_publickey_with(user, key, hash, &mut agent)
                .await
            {
                Ok(result) => {
                    let outcome = Outcome::from(result);
                    if !matches!(outcome, Outcome::Failure { partial: false, .. }) {
                        return Some(outcome);
                    }
                }
                Err(e) => warn!("ssh agent signing failed: {e}"),
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Broker server

struct Shared {
    settings: Settings,
    handle: OnceLock<SshHandle>,
    connected_at: OnceLock<Instant>,
    shutdown: Notify,
    attach: Mutex<Option<oneshot::Sender<(ConnReader, ConnWriter)>>>,
}

impl Shared {
    fn ready_handle(&self) -> Option<&SshHandle> {
        self.handle.get().filter(|h| !h.is_closed())
    }

    fn status(&self) -> StatusInfo {
        StatusInfo {
            ready: self.ready_handle().is_some(),
            pid: std::process::id(),
            protocol: ipc::PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            target: self.settings.target(),
            connected_secs: self.connected_at.get().map(|t| t.elapsed().as_secs()),
        }
    }
}

/// Who answers authentication prompts for a broker.
pub enum AuthVia {
    /// This process's terminal (`ghm up --foreground`).
    Terminal,
    /// A `ghm up` process that attaches over IPC (background broker).
    Attach,
}

/// Run a broker until the SSH connection drops or `ghm down` asks it to stop.
pub async fn run(context: &SshContext, via: AuthVia) -> AppResult<()> {
    let settings = Settings::from_context(context)?;
    let endpoint = ipc::endpoint(context)?;
    let mut listener = ipc::Listener::bind(&endpoint)
        .await
        .map_err(|e| AppError::Io(format!("cannot listen on {endpoint}: {e}")))?;
    let (attach_tx, attach_rx) = oneshot::channel();
    let shared = Arc::new(Shared {
        settings,
        handle: OnceLock::new(),
        connected_at: OnceLock::new(),
        shutdown: Notify::new(),
        attach: Mutex::new(Some(attach_tx)),
    });
    let accept_shared = shared.clone();
    let acceptor = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(io) => {
                    tokio::spawn(serve_connection(io, accept_shared.clone()));
                }
                Err(e) => {
                    warn!("IPC accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    let result = authenticate_and_serve(&shared, via, attach_rx).await;
    acceptor.abort();
    let _ = acceptor.await; // drops the listener, removing a Unix socket
    result
}

async fn authenticate_and_serve(
    shared: &Arc<Shared>,
    via: AuthVia,
    attach_rx: oneshot::Receiver<(ConnReader, ConnWriter)>,
) -> AppResult<()> {
    let handle = match via {
        AuthVia::Terminal => {
            shared.attach.lock().unwrap().take();
            interruptible(establish(&shared.settings, &mut TerminalPrompter)).await?
        }
        AuthVia::Attach => {
            let (reader, writer) = tokio::time::timeout(ATTACH_TIMEOUT, attach_rx)
                .await
                .map_err(|_| AppError::Io("no ghm up terminal attached to authenticate".into()))?
                .map_err(|_| AppError::Io("authentication relay closed".into()))?;
            let mut relay = RelayPrompter { reader, writer };
            match establish(&shared.settings, &mut relay).await {
                Ok(handle) => {
                    // The terminal may have gone; the connection is still good.
                    let _ = relay.send(&Reply::Ready).await;
                    handle
                }
                Err(e) => {
                    let _ = relay
                        .send(&Reply::AuthFailed {
                            message: e.to_string(),
                        })
                        .await;
                    return Err(e);
                }
            }
        }
    };
    let _ = shared.connected_at.set(Instant::now());
    let _ = shared.handle.set(handle);
    info!("connected to {}", shared.settings.target());
    if matches!(via, AuthVia::Terminal) {
        eprintln!(
            "Connected to {}. Keep this terminal open; press Ctrl-C to disconnect.",
            shared.settings.target()
        );
    }

    let handle = shared.handle.get().expect("handle just set");
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let interrupted = async {
        // A detached broker may have no console to take Ctrl-C from.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(interrupted);
    loop {
        tokio::select! {
            _ = shared.shutdown.notified() => {
                info!("shutdown requested");
                break;
            }
            _ = &mut interrupted => {
                info!("interrupted");
                break;
            }
            _ = tick.tick() => {
                if handle.is_closed() {
                    warn!("SSH connection to {} closed", shared.settings.target());
                    return Err(AppError::SshSpawn("SSH connection closed".into()));
                }
            }
        }
    }
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    Ok(())
}

async fn serve_connection(io: ipc::BoxedIo, shared: Arc<Shared>) {
    let (mut reader, mut writer) = ipc::split(io);
    let hello: Hello = match tokio::time::timeout(REPLY_TIMEOUT, reader.control()).await {
        Ok(Ok(Some(hello))) => hello,
        _ => return,
    };
    // Status and Shutdown stay compatible across versions so an old broker can
    // always be inspected and stopped.
    let incompatible = hello.protocol != ipc::PROTOCOL;
    let reply = match hello.request {
        Request::Status => Reply::Status(shared.status()),
        Request::Shutdown => {
            shared.shutdown.notify_one();
            Reply::Ok
        }
        _ if incompatible => Reply::Error {
            message: format!(
                "the running ghm connection (protocol {}) does not match this ghm (protocol {}); run `ghm down` then `ghm up`",
                ipc::PROTOCOL,
                hello.protocol
            ),
        },
        Request::Attach => match shared.attach.lock().unwrap().take() {
            Some(tx) => {
                let _ = tx.send((reader, writer));
                return;
            }
            None => Reply::Error {
                message: "this connection is not waiting for authentication".into(),
            },
        },
        Request::Exec { command, pty } => match shared.ready_handle() {
            Some(handle) => {
                exec(handle, &command, pty, reader, writer).await;
                return;
            }
            None => Reply::Error {
                message: "the cluster connection is not authenticated yet".into(),
            },
        },
    };
    let _ = writer.control(&reply).await;
}

async fn exec(
    handle: &SshHandle,
    command: &str,
    pty: bool,
    mut reader: ConnReader,
    mut writer: ConnWriter,
) {
    let result: AppResult<()> = async {
        let io_err = |e: std::io::Error| AppError::Io(format!("IPC write failed: {e}"));
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(ssh_err("cannot open SSH channel"))?;
        if pty {
            channel
                .request_pty(true, "xterm", 120, 40, 0, 0, &[])
                .await
                .map_err(ssh_err("pty request"))?;
        }
        channel
            .exec(true, command)
            .await
            .map_err(ssh_err("exec request"))?;
        // Stdin is forwarded by its own task so a large upload waiting on the
        // SSH window never stops output from being drained (which would stall
        // the whole shared connection). The task also notices the client
        // going away, which stops the remote command.
        let (gone_tx, mut gone_rx) = oneshot::channel::<()>();
        let mut stdin = channel.make_writer();
        let stdin_task = tokio::spawn(async move {
            loop {
                match reader.frame().await {
                    Ok(Some(Frame::Stdin(data))) => {
                        if stdin.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Ok(Some(Frame::StdinEof)) => {
                        let _ = stdin.shutdown().await;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            // Reached only on disconnect or a failed stdin write.
            let _ = gone_tx.send(());
            std::future::pending::<()>().await;
        });
        let mut code = None;
        let mut signal = None;
        let finished = loop {
            tokio::select! {
                msg = channel.wait() => match msg {
                    Some(ChannelMsg::Data { data }) => writer.stdout(&data).await.map_err(io_err)?,
                    Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                        writer.stderr(&data).await.map_err(io_err)?
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => code = Some(exit_status),
                    Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                        signal = Some(format!("{signal_name:?}"))
                    }
                    Some(ChannelMsg::Failure) => {
                        stdin_task.abort();
                        return Err(AppError::SshSpawn(
                            "the server refused the command request".into(),
                        ));
                    }
                    Some(ChannelMsg::Close) | None => break true,
                    Some(_) => {}
                },
                // The client went away (timeout, cancellation): stop the command.
                _ = &mut gone_rx => break false,
            }
        };
        stdin_task.abort();
        if !finished {
            let _ = channel.signal(Sig::TERM).await;
            let _ = channel.close().await;
            return Ok(());
        }
        writer
            .control(&Reply::Exit { code, signal })
            .await
            .map_err(io_err)
    }
    .await;
    if let Err(e) = result {
        let _ = writer
            .control(&Reply::Error {
                message: e.to_string(),
            })
            .await;
    }
}

// ---------------------------------------------------------------------------
// Client side

async fn reply(reader: &mut ConnReader) -> Result<Reply, ConnectError> {
    match tokio::time::timeout(REPLY_TIMEOUT, reader.control::<Reply>()).await {
        Ok(Ok(Some(reply))) => Ok(reply),
        Ok(Ok(None)) => Err(ConnectError::Unresponsive("connection closed".into())),
        Ok(Err(e)) => Err(ConnectError::Unresponsive(e.to_string())),
        Err(_) => Err(ConnectError::Unresponsive("no reply".into())),
    }
}

pub async fn status(endpoint: &Endpoint) -> Result<StatusInfo, ConnectError> {
    let (mut reader, _writer) = ipc::request(endpoint, Request::Status).await?;
    match reply(&mut reader).await? {
        Reply::Status(info) => Ok(info),
        other => Err(ConnectError::Unresponsive(format!(
            "unexpected reply {other:?}"
        ))),
    }
}

pub async fn check(context: &SshContext) -> AppResult<MasterState> {
    let endpoint = ipc::endpoint(context)?;
    match status(&endpoint).await {
        Ok(info) if info.ready && info.protocol == ipc::PROTOCOL => Ok(MasterState::Running),
        Ok(info) if info.ready => Ok(MasterState::Stale),
        // Still authenticating: not usable yet.
        Ok(_) => Ok(MasterState::Missing),
        Err(ConnectError::NotRunning) => Ok(MasterState::Missing),
        Err(ConnectError::Unresponsive(_)) => Ok(MasterState::Stale),
        Err(e) => Err(e.into()),
    }
}

pub async fn shutdown(context: &SshContext) -> AppResult<()> {
    let endpoint = ipc::endpoint(context)?;
    let (mut reader, _writer) = match ipc::request(&endpoint, Request::Shutdown).await {
        Ok(conn) => conn,
        Err(ConnectError::NotRunning) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    match reply(&mut reader).await? {
        Reply::Ok => {}
        other => {
            return Err(AppError::SshSpawn(format!(
                "unexpected reply to shutdown: {other:?}"
            )));
        }
    }
    // Wait for it to go away so a following `ghm up` can bind the endpoint.
    for _ in 0..50 {
        if matches!(status(&endpoint).await, Err(ConnectError::NotRunning)) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(AppError::SshSpawn(
        "the cluster connection did not stop within 5 seconds".into(),
    ))
}

pub struct UpOptions {
    pub foreground: bool,
    pub config_path: PathBuf,
}

/// `ghm up` for the built-in backend.
pub async fn up(context: &SshContext, options: UpOptions) -> AppResult<()> {
    let endpoint = ipc::endpoint(context)?;
    match status(&endpoint).await {
        Ok(info) if info.ready && info.protocol == ipc::PROTOCOL => {
            println!("Already connected to {} (pid {}).", info.target, info.pid);
            return Ok(());
        }
        Ok(info) if info.ready => {
            return Err(AppError::SshSpawn(format!(
                "a connection from ghm {} is running; run `ghm down` first, then `ghm up`",
                info.version
            )));
        }
        Ok(info) => {
            return Err(AppError::SshSpawn(format!(
                "another `ghm up` is authenticating (pid {}); finish or cancel it first",
                info.pid
            )));
        }
        Err(ConnectError::NotRunning | ConnectError::Unresponsive(_)) => {}
        Err(e) => return Err(e.into()),
    }
    if options.foreground {
        return run(context, AuthVia::Terminal).await;
    }

    let log = spawn_background(context, &options.config_path)?;
    interruptible(attach(&endpoint, &mut TerminalPrompter))
        .await
        .map_err(|e| AppError::SshSpawn(format!("{e}. Details: {}", log.display())))
}

/// Wait for a starting broker, then answer its authentication prompts.
pub(crate) async fn attach<P: Prompter>(endpoint: &Endpoint, prompter: &mut P) -> AppResult<()> {
    let started = Instant::now();
    let (mut reader, mut writer) = loop {
        match ipc::request(endpoint, Request::Attach).await {
            Ok(conn) => break conn,
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                return Err(AppError::SshSpawn(format!(
                    "the background connection did not start: {e}"
                )));
            }
        }
    };
    let relay_err = |e: std::io::Error| AppError::Io(format!("authentication relay failed: {e}"));
    loop {
        match reader.control::<Reply>().await.map_err(relay_err)? {
            Some(Reply::Info { text }) => prompter.info(&text).await?,
            Some(Reply::Prompt {
                name,
                instructions,
                prompts,
            }) => {
                let responses = prompter.prompt(&name, &instructions, &prompts).await?;
                writer
                    .control(&Answer::Responses { responses })
                    .await
                    .map_err(relay_err)?;
            }
            Some(Reply::Confirm { message }) => {
                let accept = prompter.confirm(&message).await?;
                writer
                    .control(&Answer::Confirm { accept })
                    .await
                    .map_err(relay_err)?;
            }
            Some(Reply::Ready) => return Ok(()),
            Some(Reply::AuthFailed { message }) | Some(Reply::Error { message }) => {
                return Err(AppError::SshSpawn(message));
            }
            Some(other) => {
                return Err(AppError::Io(format!("unexpected message {other:?}")));
            }
            None => {
                return Err(AppError::SshSpawn(
                    "the background connection exited during authentication".into(),
                ));
            }
        }
    }
}

/// Start `ghm broker` detached from this terminal; returns its log path.
fn spawn_background(context: &SshContext, config_path: &Path) -> AppResult<PathBuf> {
    let dir = home_dir()?.join(".gwu-hpc-mcp");
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Io(format!("{}: {e}", dir.display())))?;
    let log_path = dir.join(format!("broker-{}.log", context.cluster.id));
    if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 1024 * 1024) {
        let _ = std::fs::remove_file(&log_path);
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let log = options
        .open(&log_path)
        .map_err(|e| AppError::Io(format!("{}: {e}", log_path.display())))?;
    let exe = std::env::current_exe()
        .map_err(|e| AppError::Io(format!("cannot locate the ghm executable: {e}")))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--config")
        .arg(config_path)
        .arg("--cluster")
        .arg(&context.cluster.id)
        .arg("--user")
        .arg(&context.user)
        .arg("broker")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; it detaches from the terminal's
        // session so closing the terminal or Ctrl-C does not end the connection.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        command
            .spawn()
            .map_err(|e| AppError::Io(format!("cannot start background connection: {e}")))?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        // A Windows child inherits every inheritable handle, not just the
        // stdio it is given. Keep the long-lived connection from holding this
        // command's own stdout/stderr open: a caller capturing `ghm up`'s
        // output would otherwise wait forever. Spawn passes the child its own
        // inheritable copies, so clearing the flag here is safe.
        {
            use windows_sys::Win32::Foundation::{
                HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
            };
            use windows_sys::Win32::System::Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            };
            for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                // SAFETY: plain Win32 calls on this process's standard handles.
                unsafe {
                    let handle = GetStdHandle(id);
                    if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                        SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                    }
                }
            }
        }
        // Leave the terminal's job object when allowed, so closing the
        // terminal window does not kill the connection.
        command.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB);
        if command.spawn().is_err() {
            command.creation_flags(base);
            command
                .spawn()
                .map_err(|e| AppError::Io(format!("cannot start background connection: {e}")))?;
        }
    }
    Ok(log_path)
}
