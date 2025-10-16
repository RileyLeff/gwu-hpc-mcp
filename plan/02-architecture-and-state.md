Document 02: 02-architecture-and-state.md

Purpose

Define the Rust architecture for gwu-hpc-mcp, including how we manage SSH ControlMaster state robustly, execute commands (PTY/login shell), stream output, run tmux-backed sessions, and expose everything over MCP (stdio) using the Rust SDK. This document is implementation-ready: module boundaries, state machines, major types, and error model.
High-level shape

One repository with one primary binary crate gwu-hpc-mcp.
Modes:
MCP server (default): stdio MCP process spawned by the host (e.g., Codex).
Admin CLI subcommands: up, down, status, test, doctor (for interactive 2FA and local diagnostics).
Runtime config: single TOML file loaded at startup and hot‑reloaded (see 01-config-and-cluster-metadata.md).
Crate layout (proposed)

src/main.rs — CLI entry (clap). Subcommands:
serve-stdio (default): run MCP stdio server
up/down/status/test/doctor: admin operations
src/mcp.rs — MCP server wiring (rmcp SDK), tool handlers, resource/prompt registration
src/config.rs — serde models for the TOML schema, loader, watcher (notify)
src/ssh.rs — ControlMaster lifecycle + exec utilities (async ssh spawn, preflight checks)
src/sessions.rs — tmux session manager (open/exec/close), sentinel streaming
src/validate.rs — Slurm command validation and heuristics (denylist, compute markers)
src/policy.rs — resource/prompt rendering (simple template renderer)
src/errors.rs — error types, mapping to MCP-friendly responses
src/util.rs — shared helpers (tail buffers, time parsing, command building)
Dependencies (initial)

rmcp (Model Context Protocol Rust SDK)
tokio (full) + tokio-util
serde, serde_json, toml, thiserror, clap, tracing, tracing-subscriber
regex (heuristics)
notify (optional hot reload)
which (discover ssh/tmux paths)
MCP surface (summary)

Tools:
shell.exec(command, cwd?, env?, pty?=true, login_shell?=true, timeout_s?, cluster?=default)
session.open(name, cwd?, cluster?)
session.exec(name, command, timeout_s?, cluster?)
session.close(name, cluster?)
hpc.describe(cluster?) -> structured description (from TOML)
hpc.validate_slurm(command, cluster?) -> normalized args + warnings/errors/fix
Resources:
hpc://{cluster}/policy (rendered)
hpc://{cluster}/partitions (rendered or JSON block in text)
hpc://{cluster}/filesystem (rendered)
Prompts:
slurm-safety (rendered)
Process and IO constraints

MCP stdio servers must write only protocol JSON to stdout; logs must go to stderr. We will:
Use tracing with a compact fmt to stderr.
Ensure tool handlers never print to stdout directly.
Configuration and hot reload

Config path resolved in this order:
CLI flag --config
Env GWU_HPC_MCP_CONFIG
Default ~/.gwu-hpc-mcp/config.toml
Loader returns Arc<RwLock<Config>>. MCP handlers read via a read lock.
Watcher (notify) debounced (e.g., 300 ms); on change:
Parse TOML; if valid, swap in new Arc; re-render resources/prompts; increment config_version.
On parse error, keep the old config and log an error (stderr).
ControlMaster lifecycle (robust state machine)

Goal: maintain a single OpenSSH ControlMaster session per cluster/user, verified and recoverable. The MCP server does not attempt interactive 2FA; it returns a structured Needs2FA error so the user can run the CLI up command. Admin CLI is responsible for the interactive part.
State machine per cluster

enum MasterState { Missing, // No socket path present StaleSocket, // Socket exists but -O check fails Starting, // Attempting to start (subprocess running) Running { pid }, // -O check OK Needs2FA, // Start failed due to 2FA prompt (no TTY) Error { msg }, // Other unrecoverable error }
Health probe (background task)

Every N seconds (configurable, default 15s) run:
If socket path exists:
ssh -S <control_path> -p <port> -O check user@host
If rc==0: Running
Else: try -O exit; unlink socket; Missing
If no socket: Missing
When a tool needs exec and state != Running:
If Missing/StaleSocket: return Needs2FA with guidance: run: gwu-hpc-mcp up --cluster pegasus --user <id>
Do not try to spawn control master from the MCP stdio server (no interactive 2FA). Keep deterministic.
Admin CLI up behavior (interactive)

Preflight:
If control_path exists:
-O check; if nonzero: -O exit; unlink
Spawn:
ssh -MN -f (ensure background) -o ControlMaster=yes -o ControlPersist=8h -o ControlPath=… -o ServerAliveInterval/Count -p <port> user@host
Inherit stdio so user sees 2FA prompt.
Postflight:
Run -O check; if nonzero, print guidance and exit(1); else success.
down:
-O exit; unlink if still present.
status:
Print Master running (pid=…) | not running; echo control_path; show ssh -V; tmux -V.
test:
Run one exec (hostname && pwd) in default_cwd via the same ssh args.
doctor:
Checks: config path, parse ok, ssh path, tmux path, control_path dir perms, -O check result, sample rsync presence, network reachability (optional ping).
SSH exec composition and streaming

Command builder logic (per request):
cwd: cd <quoted> &&
env: export KEY='value' && for each env pair
append user command verbatim
login_shell: if true, remote_arg = bash -lc '<composed>'; else remote_arg = composed
Args:
Common: ssh -S <ControlPath> -o ControlMaster=no -p <port>
PTY: add -tt and set stderr=merged (pty)
No PTY: add -T and capture stdout/stderr separately
Tokio process:
tokio::process::Command with stdout/stderr piped
Read in 64 KiB chunks; forward to MCP as streaming logs (tool logs), track tail buffer (~4 KiB)
Timeout: use tokio::time::timeout; on expiry:
Send SIGINT to local ssh process; wait 3s; if still alive, kill()
Emit error event (timeout) and final exit with captured tail
Cancellation:
Keep a JoinHandle per request in a task map; on cancel tool call, send SIGINT to the ssh child.
tmux session management

Naming:
session_id = user-provided String (validated for safety)
Target pane "session:0.0"
Open:
ssh (no PTY): tmux new-session -d -s <name> -c <cwd> "${SHELL:-/bin/bash}"
Verify with tmux list-sessions | grep name
Track baseline history_size for delta streaming
Exec:
Compose remote command: <cmd>; printf "\nMCP_DONE:<uuid>:%s\n" $?
tmux send-keys -t <target> "<command>" C-m
Poll:
Every 500ms: display-message -p -t <target> '#{history_size}'; if increased:
capture-pane -p -J -t <target> -S -<delta_lines>
stream chunk; search for sentinel; if found, parse exit code and finish
Timeout: send-keys C-c and emit timeout error
Close: tmux kill-session -t <name>
Slurm validation and heuristics

validate_slurm():
Detect sbatch/srun variants
Parse -p/--partition and -t/--time using a small arg parser
Enforce require_partition/time_limit flags; check partition against config; parse time <= max_wall_time
Optional: inject default_partition/time_limit if absent and config permits
Return normalized args, warnings, and any fix suggestions
Heuristics:
Denylist: fail fast with DeniedByPolicy error (include denial rule)
compute_markers: if seen in a command on login (no Slurm context), either warn or block with “use Slurm” suggestion (configurable mode)
Concurrency model

Global semaphore for exec concurrency (defaults.max_concurrent_execs)
Per-session tmux ops serialized per session_id
Background tasks:
Health probe (ControlMaster)
Config watcher (optional)
All long-running work in tokio tasks; results streamed to MCP via rmcp’s streaming/logging primitives
Error model

Central enum Error with variants:
Needs2FA { cluster, hint }
MasterDown { cluster }
MasterStale { cluster }
SshSpawn { context, stderr_tail }
SshExecTimeout { duration }
Canceled
ValidationFailed { details }
DeniedByPolicy { rule, command }
TmuxError { op, stderr }
ConfigError { path, msg }
IoError { op, path }
Mapping to MCP:
Tool errors return a structured object with code, message, and details. Additionally log to context.
For Needs2FA/MasterDown, include a ready-to-copy CLI command (e.g., gwu-hpc-mcp up --cluster pegasus --user $USER)
Security considerations

No stdout prints from the server; MCP only. All diagnostics via stderr tracing or MCP context logs.
Quote all shell fragments (shlex-like quoting in Rust).
Denylist as a last-resort guard; rely mainly on Slurm validation + policy resources.
File tools (if added later) must cap size, use base64, and checksum; recommend rsync/Globus in messages for large transfers.
CLI UX (admin)

gwu-hpc-mcp up --cluster pegasus --user <id> [--config …]
Prompts for 2FA via ssh; verifies master; prints success and TTL
gwu-hpc-mcp serve-stdio [--config …]
Runs MCP stdio server; refuses to auto‑2FA; returns Needs2FA when master missing
gwu-hpc-mcp status — shows master state, control_path, ssh/tmux versions
gwu-hpc-mcp down — exits master and removes socket
gwu-hpc-mcp test — quick exec test (hostname && pwd) in default_cwd
gwu-hpc-mcp doctor — environment and config diagnostics with suggested remediations
Observability

tracing with env filter (e.g., RUST_LOG=gwu_hpc_mcp=info)
Key structured fields: cluster, user, control_path, op, duration_ms, exit_code
Extensibility notes

Multiple clusters supported now via TOML.
If later you want HTTP/SSE MCP transport, the core remains; only the server front-end changes.
To port the tmux streaming to SSH-only “attach-readonly,” swap capture-pane polling for a -tt attached reader with a sentinel; the boundary stays in sessions.rs.
Next

If this architecture looks good, I’ll send 03-mcp-surface-and-schemas.md: exact tool/resource/prompt definitions and JSON IO (request/response shapes), including validation outputs and standard error payloads.