Document 04: 04-implementation-guide.md

Purpose

Give you an implementation-ready guide: crate layout, Cargo.toml, function signatures, data types, key state machines, and a build order that an LLM/dev can follow to deliver gwu-hpc-mcp as a single Rust binary using the MCP Rust SDK (stdio).
Project layout (single binary crate)
gwu-hpc-mcp/

Cargo.toml
src/
main.rs
mcp.rs
config.rs
ssh.rs
sessions.rs
validate.rs
policy.rs
errors.rs
util.rs
plan/
00-index.md … 05-testing-and-host-integration.md (these docs)
Minimum toolchain

Rust 1.75+ (edition 2021 or 2024; pick one and stay consistent)
macOS/Linux with OpenSSH client (ssh) and tmux installed locally
Cargo.toml (initial)
[package]
name = "gwu-hpc-mcp"
version = "0.1.0"
edition = "2021"

[dependencies]

MCP stdio server
rmcp = { package = "modelcontextprotocol", version = "0.6", features = ["server"] }

async/runtime
tokio = { version = "1.39", features = ["full"] }
tokio-util = "0.7"

config + serde
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
toml = "0.8"

cli + logging
clap = { version = "4.5", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }

utils
thiserror = "1.0"
regex = "1.10"
notify = "6.1"           # hot reload watcher (optional)
which = "6.0"            # resolve ssh/tmux
shell-escape = "0.1"     # quote args safely for bash -lc
humantime = "2.1"        # display durations
humantime-serde = "1.1"  # if we later serialize durations
time = { version = "0.3", features = ["parsing", "formatting"] } # parse "14-00:00:00"

Module responsibilities and key types

src/errors.rs

Central error enum for mapping to MCP-friendly failures.
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
#[error("ControlMaster requires 2FA: run {0}")]
Needs2FA(String),
#[error("ControlMaster not running")]
MasterDown,
#[error("Stale ControlPath socket")]
MasterStale,
#[error("SSH spawn failed: {0}")]
SshSpawn(String),
#[error("SSH exec timed out after {0:?}")]
SshTimeout(std::time::Duration),
#[error("Command denied by policy: {0}")]
DeniedByPolicy(String),
#[error("Validation failed: {0}")]
ValidationFailed(String),
#[error("tmux error: {0}")]
Tmux(String),
#[error("Unknown session: {0}")]
UnknownSession(String),
#[error("Config error: {0}")]
Config(String),
#[error("IO error: {0}")]
Io(String),
}

impl AppError {
pub fn code(&self) -> &'static str { /* map to short codes / "Needs2FA" / etc. */ }
}

src/config.rs

Serde models that match 01-config-and-cluster-metadata.md schema
Loader + optional watcher
Accessors with defaults; config_version increments on reload
#[derive(Debug, serde::Deserialize, Clone)]
pub struct Config {
pub version: u32,
pub defaults: Defaults,
pub heuristics: Heuristics,
pub clusters: Vec<Cluster>,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct Defaults {
pub pty: bool,
pub login_shell: bool,
pub max_concurrent_execs: usize,
pub warn_on_login_compute: bool,
pub default_cluster: Option<String>,
pub default_cwd: Option<String>,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct Heuristics {
pub denylist: Vec<String>,
pub compute_markers: Vec<String>,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct Cluster {
pub id: String,
pub display_name: Option<String>,
pub host: String,
pub port: u16,
pub ssh: ClusterSsh,
pub access: ClusterAccess,
pub filesystems: ClusterFs,
pub slurm: ClusterSlurm,
pub prompts: ClusterPrompts,
pub policy: ClusterPolicy,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct ClusterSsh {
pub control_path: String,
pub server_alive_interval: Option<u64>,
pub server_alive_count_max: Option<u64>,
pub ssh_binary: Option<String>,
pub tmux_binary: Option<String>,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct ClusterAccess {
pub login_nodes: Vec<String>,
pub require_vpn: bool,
pub prohibit_login_compute: bool,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct ClusterFs {
pub lustre_root: String,
pub groups_root: String,
pub home_is_nfs: bool,
pub groups_is_nfs: bool,
pub scratch_recommended: bool,
pub scratch_purge_policy_url: Option<String>,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct ClusterSlurm {
pub require_partition: bool,
pub require_time_limit: bool,
pub max_wall_time: String,       // "D-HH:MM:SS"
pub default_partition: Option<String>,
pub default_time_limit: Option<String>,
pub partitions: Vec<Partition>,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct Partition {
pub name: String,
pub kind: String,                 // "cpu" | "gpu"
pub gpus: Option<Vec<String>>,
pub notes: Option<String>,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct ClusterPrompts { pub slurm_safety_template: String }

#[derive(Debug, serde::Deserialize, Clone)]
pub struct ClusterPolicy {
pub template: Option<String>,
pub template_path: Option<String>,
}

pub struct SharedConfig {
inner: std::sync::Arc<tokio::sync::RwLock<(Config, u64)>>, // (config, version)
}

impl SharedConfig {
pub async fn load(path: &std::path::Path) -> Result<Self, AppError> { /* read + parse / }
pub async fn get(&self) -> (Config, u64) { / clone / }
pub async fn replace(&self, new_cfg: Config) { / bump version */ }
}

pub async fn watch(path: std::path::PathBuf, shared: SharedConfig) { /* notify + debounce */ }

src/util.rs

TailBuffer, time parsing, shell command builders
pub struct TailBuffer { buf: bytes::BytesMut, cap: usize }
impl TailBuffer { pub fn push(&mut self, chunk: &[u8]); pub fn tail(&self) -> String; }

pub fn parse_slurm_time(s: &str) -> Optionstd::time::Duration; // "D-HH:MM:SS" | "HH:MM:SS" | "MM:SS"
pub fn duration_to_slurm(d: std::time::Duration) -> String;

pub fn quote_for_bash(s: &str) -> String { shell_escape::escape(s.into()).to_string() }

src/ssh.rs

ControlMaster manager + exec utilities
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterState { Missing, StaleSocket, Starting, Running, Needs2FA, Error }

pub struct ControlMaster {
cluster_id: String,
user: String,
host: String,
port: u16,
control_path: String,
ssh_bin: String,
keepalive: (u64, u64),
state: std::sync::Arc<tokio::sync::RwLock<MasterState>>,
}

impl ControlMaster {
pub fn new(cluster: &Cluster, user: String) -> Self { /* build from config / }
pub async fn check(&self) -> MasterState { / ssh -S ... -O check; classify / }
pub async fn ensure_running_noninteractive(&self) -> Result<(), AppError> {
// For MCP path: if Missing/Stale -> return Needs2FA("gwu-hpc-mcp up --cluster X --user Y")
}
// CLI-only:
pub async fn spawn_master_interactive(&self) -> Result<(), AppError> {
// Preflight: if socket exists but !check -> -O exit; unlink
// Spawn: ssh -MN -f -o ControlMaster=yes -o ControlPersist=8h -o ControlPath=... -o ServerAlive... -p <port> user@host
// Attach stdout/stderr to TTY (inherit) so 2FA prompts show.
// Post: check() == Running else Needs2FA/Error
}
pub async fn shutdown(&self) -> Result<(), AppError> { / -O exit; unlink if needed / }
fn control_args(&self) -> Vec<String> { / -S <path> -p <port> -o ControlPath=... */ }
}

pub struct ExecOptions {
pub command: String,
pub cwd: Option<String>,
pub env: std::collections::BTreeMap<String, String>,
pub pty: bool,
pub login_shell: bool,
pub timeout: Optionstd::time::Duration,
}

pub struct ExecResult {
pub exit_code: i32,
pub duration: std::time::Duration,
pub streams: std::collections::BTreeMap<String, String>,
pub tail: String,
pub status: Vec<String>,
}

pub async fn exec_stream<F>(
cm: &ControlMaster,
opts: ExecOptions,
mut on_chunk: F,
) -> Result<ExecResult, AppError>
where F: FnMut(&str, &str) -> tokio::task::JoinHandle<()> + Send
{
// Build remote command: cd && export && user cmd; bash -lc '...'
// Spawn tokio::process::Command with ssh args, PTY: -tt (stderr merges); no PTY: -T
// Read stdout/stderr; on each read, call on_chunk(stream, data); update TailBuffer
// Timeout: tokio::time::timeout; SIGINT then kill
}

fn build_remote_command(opts: &ExecOptions) -> String { /* quote_for_bash for each part / }
fn build_ssh_args(cm: &ControlMaster, remote_cmd: &str, pty: bool) -> Vec<String> { / -S path, -o ControlMaster=no, -tt/-T */ }

src/sessions.rs

tmux session manager using ssh.run of tmux commands (no PTY), polling for history deltas, sentinel parsing
pub struct TmuxManager { cm: ControlMaster, tmux_bin: String, poll: std::time::Duration }

impl TmuxManager {
pub fn new(cm: ControlMaster, tmux_bin: String) -> Self { /* ... / }
pub async fn open(&self, name: &str, cwd: Option<&str>) -> Result<(), AppError> {
// tmux new-session -d -s <name> -c <cwd> "${SHELL:-/bin/bash}"
}
pub async fn exec<F>(
&self, name: &str, command: &str, timeout: Optionstd::time::Duration, mut on_chunk: F
) -> Result<(i32, std::time::Duration, String), AppError>
where F: FnMut(&str) -> tokio::task::JoinHandle<()> + Send
{
// send-keys with sentinel "MCP_DONE:<uuid>:%s"
// loop: poll history_size -> capture-pane delta -> on_chunk(data)
// detect sentinel -> parse exit -> return
// on timeout: send-keys C-c; return Timeout
}
pub async fn close(&self, name: &str) -> Result<(), AppError> { / tmux kill-session */ }
}

src/validate.rs

Slurm command validator and normalizer
pub struct SlurmValidation {
pub ok: bool,
pub normalized: Option<String>,
pub partition_ok: bool,
pub time_ok: bool,
pub suggestions: Vec<String>,
pub warnings: Vec<String>,
}

pub fn validate_slurm(cmd: &str, cluster: &Cluster, inject_defaults: bool) -> Result<SlurmValidation, AppError> {
// 1) Tokenize: a simple lexer splitting on whitespace while respecting quotes.
// 2) Must start with sbatch|srun.
// 3) Extract -p/--partition and -t/--time (normalize names), remaining args keep order.
// 4) Lookup partition in cluster.slurm.partitions; suggest close matches on miss.
// 5) Parse time; compare to max_wall_time; clamp suggestion if exceeded.
// 6) If inject_defaults and missing, insert defaults; rebuild normalized line.
}

fn tokenize(command: &str) -> Vec<String> { /* simple parser or use shlex crate if preferred / }
fn rebuild(argv: &[String]) -> String { / join with proper quoting */ }

src/policy.rs

Render resources and prompts from config templates. Keep it tiny.
pub fn render_policy(cluster: &Cluster) -> String {
// Replace {{host}}, {{filesystems.lustre_root}}, {{slurm.max_wall_time}}, etc.
// Simple {{key}} substitution is enough; avoid full templating if possible.
}

pub fn render_slurm_safety(cluster: &Cluster, defaults: &Defaults) -> String { /* substitute scratch path etc. */ }

src/mcp.rs

Wire up the MCP stdio server using rmcp
Register tools/resources/prompts
Use context.log(level, msg) for streaming lines
pub async fn run_stdio(shared: crate::config::SharedConfig) -> Result<(), AppError> {
// 1) Initialize RMCP server and context
// 2) Register tools:
//    - shell.exec
//    - session.open / session.exec / session.close
//    - hpc.describe / hpc.validate_slurm
//    - health.check (optional)
// 3) Register resources and prompts for each cluster (call policy renderers)
// 4) Start serving on stdio
}

Tool handler patterns (signatures)

shell.exec input/output structs:

#[derive(serde::Deserialize)]
pub struct ShellExecIn {
pub command: String,
pub cwd: Option<String>,
pub env: Option<std::collections::BTreeMap<String, String>>,
pub pty: Option<bool>,
pub login_shell: Option<bool>,
pub timeout_s: Option<u64>,
pub cluster: Option<String>,
}

#[derive(serde::Serialize)]
pub struct ShellExecOut {
pub ok: bool,
pub exit_code: Option<i32>,
pub duration_ms: Option<u128>,
pub streams: std::collections::BTreeMap<String, String>,
pub tail: String,
pub status: Vec<String>,
pub code: Option<String>,    // error code when ok=false
pub message: Option<String>, // error message when ok=false
pub details: Option<serde_json::Value>,
}

Handler flow:

Resolve cluster (or defaults.default_cluster), user (env USER or GWU_HPC_USER env override if we add it).
Build ControlMaster from cluster + user; call ensure_running_noninteractive():
If Needs2FA -> return ok=false with code="Needs2FA" and a CLI hint string
Build ExecOptions merging defaults (pty/login_shell/default_cwd) and request overrides.
Stream: for each chunk, context.log("info", chunk) (split by lines to avoid half-glyph logs).
On completion, return ShellExecOut with accumulated fields.
session.* handlers:

Keep a map<cluster+session_name, SessionState> with baseline history_size if desired (or compute each time).
Use TmuxManager to open/exec/close; stream via context.log.
hpc.describe:

Read current Config; return the selected cluster struct (or a summarized subset) plus config_version.
hpc.validate_slurm:

Call validate_slurm(); return structured result or ValidationFailed error.
health.check:

Report MasterState (via check()) and control_path string.
src/main.rs

Clap-based CLI: default subcommand = serve-stdio
Shared flags: --config <path>, --cluster <id>, --user <id>, --log-level
#[derive(clap::Parser)]
#[command(name = "gwu-hpc-mcp", version, about)]
pub struct Cli {
#[command(subcommand)]
cmd: Option<Cmd>,
#[arg(long)]
config: Optionstd::path::PathBuf,
#[arg(long)]
cluster: Option<String>,
#[arg(long)]
user: Option<String>,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
/// Run MCP stdio server
ServeStdio,
/// Establish ControlMaster interactively (2FA)
Up,
/// Shut down ControlMaster
Down,
/// Show ControlMaster status
Status,
/// Quick exec test over SSH
Test,
/// Diagnostics
Doctor,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
// init tracing subscriber from RUST_LOG or default
// resolve config path, load SharedConfig, optionally start watcher
// match cmd:
//  - ServeStdio => mcp::run_stdio(shared).await
//  - Up/Down/Status/Test/Doctor => call ssh::ControlMaster helpers or small flows
}

Build order (recommended)

Skeleton + config
Create crate, Cargo.toml, main.rs with Clap; implement config::Config structs and loader.
Hard-code a minimal config.toml path and ensure it parses.
2. Errors + logging

Implement AppError and basic tracing setup.
3. ControlMaster manager

ssh::ControlMaster::check(), spawn_master_interactive(), shutdown()
Implement CLI up/down/status; verify in real environment (2FA prompt shows; status prints “running”).
4. Exec streaming

Implement build_remote_command(), build_ssh_args(), exec_stream() with timeout/cancel support.
Add CLI test subcommand to run hostname && pwd using exec_stream() and print results.
5. Tmux manager

Implement open/exec/close with sentinel and capture-pane polling.
Manual test: open -> exec "echo hi" -> close.
6. Validate Slurm

Implement validate_slurm() and helpers; unit tests for parsing and normalization.
7. Policy rendering

Implement render_policy() and render_slurm_safety(); smoke test with your TOML.
8. MCP server

Wire rmcp stdio server; register tools/resources/prompts.
Implement tool handlers with streaming via context.log.
Manual test with MCP Inspector or Codex CLI.
9. Hot reload (optional first cut)

Add notify watcher; on change, parse and replace config; re-register resources/prompts.
10. Hardening + polish

Denylist enforcement, compute_markers warnings, concurrency semaphore
doctor: report ssh path, tmux path, ControlPath dir, -O check, quick network check
Testing suggestions (unit stubs)

parse_slurm_time() round-trips typical forms
validate_slurm() covers: missing -p/-t, unknown partition, time > max, inject_defaults
build_remote_command() quotes env and cwd correctly
TailBuffer keeps last N bytes
ControlMaster::check() output parser (mock Command)
Integration test harness (fake ssh)

Provide a small “fake ssh” test binary behind a test feature:
Emulates -O check exit codes
For exec, echoes canned stdout with a delay
For tmux commands, returns plausible outputs
Point ssh_binary to this fake in tests via config override
Security & safety notes

Never write to stdout except via rmcp server protocol. All logs to stderr.
Quote every shell fragment; prefer passing argv to ssh and only one quoted remote command segment.
Enforce denylist before spawning ssh. Provide clear error messages.
Keep PTY default true; allow disabling for strict stdout/stderr separation.
CLI snippets (for users)

Establish master (interactive): gwu-hpc-mcp up --cluster pegasus --user $USER
Run server for Codex (stdio): gwu-hpc-mcp serve-stdio
Status: gwu-hpc-mcp status
Test: gwu-hpc-mcp test
Down: gwu-hpc-mcp down
Host integration (Codex) — preview
~/.codex/config.toml:
startup_timeout_sec = 30
[mcp_servers.gwu_hpc]
command = "/usr/local/bin/gwu-hpc-mcp"
args = ["serve-stdio", "--config", "/Users/you/.gwu-hpc-mcp/config.toml"]
env = { GWU_HPC_MCP_USER = "yourid" }

Next

I’ll deliver 05-testing-and-host-integration.md with concrete tests (unit/integration/e2e), a fake-ssh harness, and Codex config guidance, plus a simple release checklist.