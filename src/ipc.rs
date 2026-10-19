//! Local IPC between `ghm` processes and the built-in SSH broker.
//!
//! Unix: a socket in an owner-only directory; both ends check the peer UID.
//! Windows: a named pipe with an owner-only DACL that rejects remote clients;
//! both ends check that the peer process runs as the same user SID. The client
//! check matters because `ghm up` relays passwords and 2FA answers over it.
use std::io;

use bytes::{BufMut, Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::errors::{AppError, AppResult};
use crate::ssh::SshContext;

/// Bumped on incompatible changes so an old broker is reported, not misused.
pub const PROTOCOL: u32 = 1;
const MAX_FRAME: usize = 1024 * 1024;
pub const DATA_CHUNK: usize = 32 * 1024;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}
pub type BoxedIo = Box<dyn Io>;

// ---------------------------------------------------------------------------
// Messages

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub request: Request,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Status,
    Shutdown,
    /// Drive authentication from the `ghm up` terminal.
    Attach,
    Exec {
        command: String,
        pty: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub ready: bool,
    pub pid: u32,
    pub protocol: u32,
    pub version: String,
    pub target: String,
    pub connected_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptItem {
    pub text: String,
    pub echo: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Status(StatusInfo),
    Ok,
    Error {
        message: String,
    },
    Exit {
        code: Option<u32>,
        signal: Option<String>,
    },
    // Authentication relay (only on an Attach connection).
    Info {
        text: String,
    },
    Prompt {
        name: String,
        instructions: String,
        prompts: Vec<PromptItem>,
    },
    Confirm {
        message: String,
    },
    Ready,
    AuthFailed {
        message: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Responses { responses: Vec<String> },
    Confirm { accept: bool },
}

// ---------------------------------------------------------------------------
// Framing: a length-delimited frame whose first byte says what it carries.

const TAG_CONTROL: u8 = 0;
const TAG_STDOUT: u8 = 1;
const TAG_STDERR: u8 = 2;
const TAG_STDIN: u8 = 3;
const TAG_STDIN_EOF: u8 = 4;

#[derive(Debug)]
pub enum Frame {
    Control(Bytes),
    Stdout(Bytes),
    Stderr(Bytes),
    Stdin(Bytes),
    StdinEof,
}

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME)
        .new_codec()
}

pub struct ConnReader(FramedRead<ReadHalf<BoxedIo>, LengthDelimitedCodec>);
pub struct ConnWriter(FramedWrite<WriteHalf<BoxedIo>, LengthDelimitedCodec>);

pub fn split(io: BoxedIo) -> (ConnReader, ConnWriter) {
    let (r, w) = tokio::io::split(io);
    (
        ConnReader(FramedRead::new(r, codec())),
        ConnWriter(FramedWrite::new(w, codec())),
    )
}

impl ConnReader {
    /// Next frame, or None when the peer has gone away.
    pub async fn frame(&mut self) -> io::Result<Option<Frame>> {
        let Some(buf) = self.0.next().await.transpose()? else {
            return Ok(None);
        };
        let mut buf = buf.freeze();
        if buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty frame"));
        }
        let tag = buf[0];
        let body = buf.split_off(1);
        Ok(Some(match tag {
            TAG_CONTROL => Frame::Control(body),
            TAG_STDOUT => Frame::Stdout(body),
            TAG_STDERR => Frame::Stderr(body),
            TAG_STDIN => Frame::Stdin(body),
            TAG_STDIN_EOF => Frame::StdinEof,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown frame tag {other}"),
                ));
            }
        }))
    }

    /// Next control message; data frames here are a protocol error.
    pub async fn control<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Option<T>> {
        match self.frame().await? {
            None => Ok(None),
            Some(Frame::Control(body)) => parse(&body).map(Some),
            Some(other) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected control frame, got {other:?}"),
            )),
        }
    }
}

pub fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> io::Result<T> {
    serde_json::from_slice(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

impl ConnWriter {
    pub async fn control<T: Serialize>(&mut self, msg: &T) -> io::Result<()> {
        let json = serde_json::to_vec(msg).map_err(io::Error::other)?;
        self.tagged(TAG_CONTROL, &json).await
    }

    pub async fn stdout(&mut self, data: &[u8]) -> io::Result<()> {
        self.tagged(TAG_STDOUT, data).await
    }

    pub async fn stderr(&mut self, data: &[u8]) -> io::Result<()> {
        self.tagged(TAG_STDERR, data).await
    }

    pub async fn stdin(&mut self, data: &[u8]) -> io::Result<()> {
        for chunk in data.chunks(DATA_CHUNK) {
            self.tagged(TAG_STDIN, chunk).await?;
        }
        Ok(())
    }

    pub async fn stdin_eof(&mut self) -> io::Result<()> {
        self.tagged(TAG_STDIN_EOF, &[]).await
    }

    async fn tagged(&mut self, tag: u8, body: &[u8]) -> io::Result<()> {
        let mut buf = BytesMut::with_capacity(body.len() + 1);
        buf.put_u8(tag);
        buf.extend_from_slice(body);
        self.0.send(buf.freeze()).await
    }
}

// ---------------------------------------------------------------------------
// Endpoints

#[derive(Debug, Clone)]
pub struct Endpoint {
    #[cfg(unix)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    name: String,
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(unix)]
        return write!(f, "{}", self.path.display());
        #[cfg(windows)]
        return write!(f, "{}", self.name);
    }
}

/// The broker endpoint for this cluster connection. On Unix it sits next to
/// the configured ControlPath; on Windows it is a per-user named pipe.
pub fn endpoint(context: &SshContext) -> AppResult<Endpoint> {
    #[cfg(unix)]
    {
        let mut path = context.control_path()?.into_os_string();
        path.push(".broker");
        Ok(Endpoint { path: path.into() })
    }
    #[cfg(windows)]
    {
        let local = std::env::var("USERNAME").unwrap_or_default();
        let raw = format!(
            "{local}-{}@{}_{}",
            context.user, context.cluster.host, context.cluster.port
        );
        let safe: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "@._-".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Ok(Endpoint {
            name: format!(r"\\.\pipe\gwu-hpc-mcp-{safe}"),
        })
    }
}

#[derive(Debug)]
pub enum ConnectError {
    /// Nothing is listening.
    NotRunning,
    /// An endpoint exists but nobody answers (crashed broker, stale socket).
    Unresponsive(String),
    Other(io::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NotRunning => write!(f, "cluster connection is not running"),
            ConnectError::Unresponsive(e) => write!(f, "cluster connection is unresponsive: {e}"),
            ConnectError::Other(e) => write!(f, "cannot reach cluster connection: {e}"),
        }
    }
}

impl From<ConnectError> for AppError {
    fn from(e: ConnectError) -> Self {
        AppError::SshSpawn(e.to_string())
    }
}

/// Connect, verify the broker belongs to this user, and send the request.
pub async fn request(
    endpoint: &Endpoint,
    request: Request,
) -> Result<(ConnReader, ConnWriter), ConnectError> {
    let io = connect(endpoint).await?;
    let (reader, mut writer) = split(io);
    writer
        .control(&Hello {
            protocol: PROTOCOL,
            request,
        })
        .await
        .map_err(|e| ConnectError::Unresponsive(e.to_string()))?;
    Ok((reader, writer))
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tokio::net::{UnixListener, UnixStream};

    fn uid() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    fn same_user(stream: &UnixStream) -> io::Result<()> {
        let cred = stream.peer_cred()?;
        if cred.uid() == uid() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("peer uid {} is not the current user", cred.uid()),
            ))
        }
    }

    pub async fn connect(endpoint: &Endpoint) -> Result<BoxedIo, ConnectError> {
        match UnixStream::connect(&endpoint.path).await {
            Ok(stream) => {
                same_user(&stream).map_err(ConnectError::Other)?;
                Ok(Box::new(stream))
            }
            // Refused: a socket file left by a broker that exited without
            // cleaning up; nothing is listening, and the next bind replaces it.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Err(ConnectError::NotRunning)
            }
            Err(e) => Err(ConnectError::Other(e)),
        }
    }

    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        pub async fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let path = endpoint.path.clone();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
            if path.exists() {
                if UnixStream::connect(&path).await.is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "a cluster connection broker is already running",
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            let inner = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self { inner, path })
        }

        pub async fn accept(&mut self) -> io::Result<BoxedIo> {
            loop {
                let (stream, _) = self.inner.accept().await?;
                match same_user(&stream) {
                    Ok(()) => return Ok(Box::new(stream)),
                    Err(e) => tracing::warn!("rejected IPC client: {e}"),
                }
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::time::{Duration, Instant};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeServer, PipeMode, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// The SID of the user a process runs as, as raw bytes plus SDDL string.
    fn process_sid(process: HANDLE) -> io::Result<(Vec<u8>, String)> {
        // SAFETY: standard token query; every handle and buffer is checked and
        // released before return. The u64 buffer keeps TOKEN_USER aligned.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            let bytes = std::slice::from_raw_parts(sid as *const u8, GetLengthSid(sid) as usize);
            let mut wide: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut wide) == 0 {
                return Err(io::Error::last_os_error());
            }
            let n = (0..).take_while(|&i| *wide.add(i) != 0).count();
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, n));
            LocalFree(wide.cast());
            Ok((bytes.to_vec(), text))
        }
    }

    fn current_sid() -> io::Result<(Vec<u8>, String)> {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no close.
        process_sid(unsafe { GetCurrentProcess() })
    }

    fn pid_sid(pid: u32) -> io::Result<Vec<u8>> {
        // SAFETY: the process handle is closed after the query.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            let result = process_sid(process);
            CloseHandle(process);
            result.map(|(bytes, _)| bytes)
        }
    }

    fn check_peer(pid: u32) -> io::Result<()> {
        if pid_sid(pid)? == current_sid()?.0 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("pipe peer process {pid} runs as a different user"),
            ))
        }
    }

    pub async fn connect(endpoint: &Endpoint) -> Result<BoxedIo, ConnectError> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let client = loop {
            match ClientOptions::new().open(&endpoint.name) {
                Ok(client) => break client,
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                    return Err(ConnectError::NotRunning);
                }
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if Instant::now() > deadline {
                        return Err(ConnectError::Unresponsive(e.to_string()));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(ConnectError::Other(e)),
            }
        };
        let mut pid = 0u32;
        // SAFETY: the handle is a live pipe client owned by `client`.
        if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut pid) } == 0 {
            return Err(ConnectError::Other(io::Error::last_os_error()));
        }
        check_peer(pid).map_err(ConnectError::Other)?;
        Ok(Box::new(client))
    }

    /// Owner-only security descriptor, freed on drop.
    struct Security {
        descriptor: PSECURITY_DESCRIPTOR,
    }

    unsafe impl Send for Security {}
    unsafe impl Sync for Security {}

    impl Security {
        fn owner_only() -> io::Result<Self> {
            let (_, sid) = current_sid()?;
            // Protected DACL granting full access to this user only.
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: sddl is NUL-terminated; descriptor is released in Drop.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { descriptor })
        }

        fn create(&self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor,
                bInheritHandle: 0,
            };
            // SAFETY: attrs and the descriptor outlive the call.
            unsafe {
                ServerOptions::new()
                    .first_pipe_instance(first)
                    .reject_remote_clients(true)
                    .pipe_mode(PipeMode::Byte)
                    .create_with_security_attributes_raw(
                        name,
                        &mut attrs as *mut SECURITY_ATTRIBUTES as *mut c_void,
                    )
            }
        }
    }

    impl Drop for Security {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe {
                LocalFree(self.descriptor);
            }
        }
    }

    pub struct Listener {
        name: String,
        security: Security,
        next: NamedPipeServer,
    }

    impl Listener {
        pub async fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let security = Security::owner_only()?;
            // first_pipe_instance fails if anyone (any user) already owns the name.
            let next = security.create(&endpoint.name, true).map_err(|e| {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "a cluster connection broker is already running (or the pipe name is taken)",
                    )
                } else {
                    e
                }
            })?;
            Ok(Self {
                name: endpoint.name.clone(),
                security,
                next,
            })
        }

        pub async fn accept(&mut self) -> io::Result<BoxedIo> {
            loop {
                self.next.connect().await?;
                let fresh = self.security.create(&self.name, false)?;
                let connected = std::mem::replace(&mut self.next, fresh);
                let mut pid = 0u32;
                // SAFETY: the handle is a live, connected pipe server instance.
                let ok = unsafe {
                    GetNamedPipeClientProcessId(connected.as_raw_handle() as HANDLE, &mut pid)
                };
                match if ok == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    check_peer(pid)
                } {
                    Ok(()) => return Ok(Box::new(connected)),
                    Err(e) => tracing::warn!("rejected IPC client: {e}"),
                }
            }
        }
    }
}

pub use platform::{Listener, connect};
