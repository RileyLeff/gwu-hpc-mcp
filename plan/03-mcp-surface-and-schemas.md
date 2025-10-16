Document 03: 03-mcp-surface-and-schemas.md

Purpose

Lock the exact MCP surface (tools, resources, prompts) and the JSON IO shapes each tool returns.
Define streaming behavior, error payloads, and cluster selection rules so an LLM agent can use the server deterministically.
Transport and server

Transport: stdio MCP using the official Rust SDK (modelcontextprotocol/rust-sdk; “rmcp” crate). The server writes only MCP protocol JSON to stdout; logs go to stderr. The SDK provides tool registration, resource publishing, and context logging. [Reference: Rust SDK] (github.com/modelcontextprotocol/rust-sdk)
Cluster selection

Most tools accept an optional cluster parameter. If omitted, the server uses defaults.default_cluster from the TOML config (see 01-config-and-cluster-metadata.md).
Tool handlers validate the cluster id against the loaded config; unknown ids return an error with a list of known clusters.
Error model (common to all tools)

On failure, tools return a JSON object with:
ok: false
code: string (e.g., "Needs2FA", "MasterDown", "ValidationFailed", "DeniedByPolicy", "Timeout", "TmuxError", "ConfigError")
message: human-readable message
details: object with extra fields (optional)
On success, tools return ok: true with result fields.
Tool functions also emit context logs (info/warn/error) while streaming.
Streaming behavior

For long-running operations (shell.exec, session.exec) the server:
Streams output chunks as MCP log entries via the SDK’s logging functions (line-terminated where possible). Chunk size target ~64 KiB.
Emits a final result object when the process (or tmux command) completes.
Maintains an internal “tail” buffer (~4096 bytes) and includes it in the final result as tail to aid UI summaries.
Tools

shell.exec
Purpose: Execute a command on the remote cluster through the established ControlMaster.
Input:
command: string (required) — user command to run
cwd: string (optional) — working directory; default from config defaults.default_cwd
env: object map[string]string (optional) — merged into remote environment
pty: boolean (optional; default true) — if true, allocate a remote PTY (-tt); stderr merges into stdout
login_shell: boolean (optional; default true) — if true, wrap in bash -lc '...'
timeout_s: number (optional) — seconds; if exceeded, server sends SIGINT then kill after grace
cluster: string (optional) — cluster id; default to defaults.default_cluster
Behavior:
Composition: [cd cwd] && [export K=V]… && command; wrap with bash -lc if login_shell.
Preflight validation:
If the command matches heuristics.compute_markers and looks “compute-like” on login nodes, emit a warning suggesting sbatch/srun per policy. [Pegasus policy: compute must run under Slurm; login nodes are for submission/data movement/build] (hpc.gwu.edu)
Denylist strings cause immediate error (ok=false, code="DeniedByPolicy").
Streaming: outputs arrive as info logs; warnings/errors as warn/error logs.
Output (final object):
ok: true
exit_code: integer
duration_ms: integer
streams: object map[string]string — concatenated stream texts by name ("pty" or "stdout"/"stderr")
tail: string — last ~4 KiB of merged output
status: array[string] — status/warning messages encountered
Errors:
Needs2FA if no ControlMaster (include a copyable CLI command to run up)
MasterDown/MasterStale, Timeout, SshSpawn, ValidationFailed, DeniedByPolicy
Example (abridged):
Input:
{ "command": "hostname && sinfo | head -n 5", "cwd": "/lustre/groups/gedanlab/rileyleff", "timeout_s": 60 }

Output (final):
{ "ok": true, "exit_code": 0, "duration_ms": 1240, "streams": { "pty": "pegasus-log001\nPARTITION ...\n" }, "tail": "...\n", "status": [] }

2. session.open

Purpose: Create a persistent tmux session for interactive/persistent work.
Input:
name: string (required) — session identifier (validated)
cwd: string (optional) — initial working directory
cluster: string (optional)
Behavior:
ssh (no PTY): tmux new-session -d -s <name> -c <cwd> "${SHELL:-/bin/bash}"
Verify session exists; initialize history baseline for streaming deltas.
Output:
ok: true
session: string — normalized session name
message: string — confirmation
Errors: TmuxError, Needs2FA, MasterDown
3. session.exec

Purpose: Run a command inside an existing tmux session and stream the pane output until a sentinel is observed.
Input:
name: string (required)
command: string (required)
timeout_s: number (optional)
cluster: string (optional)
Behavior:
Send: <command>; printf "\nMCP_DONE:<uuid>:%s\n" $?
Poll history_size every 500 ms; capture-pane deltas; stream chunks; detect sentinel; parse exit code.
On timeout: send Ctrl-C (C-c) and return ok=false, code="Timeout".
Output:
ok: true
exit_code: integer
duration_ms: integer
streams: { "tmux": "..." }
tail: string
Errors: UnknownSession, Timeout, Needs2FA, MasterDown, TmuxError
4. session.close

Purpose: Terminate a tmux session.
Input:
name: string (required)
cluster: string (optional)
Output:
ok: true
message: string
5. hpc.describe

Purpose: Return structured metadata about the selected cluster for agent reasoning.
Input:
cluster: string (optional)
Output (example shape):
ok: true
cluster: { id, display_name, host, port, access: { login_nodes: [...], require_vpn: bool, prohibit_login_compute: bool }, filesystems: { lustre_root: "/lustre", groups_root: "/lustre/groups", home_is_nfs: true, groups_is_nfs: true, scratch_recommended: true, scratch_purge_policy_url: "..." }, slurm: { require_partition: true, require_time_limit: true, max_wall_time: "14-00:00:00", default_partition: "defq", default_time_limit: "01:00:00", partitions: [{ name: "defq", kind: "cpu" }, { name: "small-gpu", kind: "gpu" }, ...] }, prompts: { slurm_safety_template: "..." }, policy_version: 1, config_version: integer }
6. hpc.validate_slurm

Purpose: Validate and normalize a proposed Slurm command (sbatch/srun).
Input:
command: string (required)
cluster: string (optional)
inject_defaults: boolean (optional; default false) — if true, insert default -p/-t when missing, respecting caps
Behavior:
Parse arg vector; detect sbatch/srun; extract -p/--partition and -t/--time; verify against config.
Confirm partition exists; if not, suggest close matches.
Parse time and compare to max_wall_time; on exceed, suggest capped time.
If inject_defaults, add defaults when missing.
Output:
ok: true
normalized_command: string — reconstructed sbatch/srun line with normalized flags (if inject_defaults or if original valid)
partition_ok: boolean
time_ok: boolean
suggestions: array[string] — specific fixes (“Add -p defq”, “Reduce -t to 14-00:00:00”)
warnings: array[string] — policy nudges (e.g., “Consider small-gpu for GPU requests”)
Errors:
ValidationFailed with details if the command is not sbatch/srun or contains unrecognized/invalid flags that prevent normalization.
Resources

hpc://{cluster}/policy
Rendered from clusters.policy.template with variables bound from the cluster entry (host, login_nodes, slurm.max_wall_time, filesystems.*). Content reflects Pegasus guidance: submit compute via Slurm with -p/-t (max 14 days); do not run compute on login nodes; use Lustre scratch (/lustre, /lustre/groups) rather than home/groups (NFS). [Pegasus policy references] (hpc.gwu.edu)
hpc://{cluster}/partitions
Rendered summary of partitions with brief notes (kind, known GPU models if specified).
hpc://{cluster}/filesystem
Short guide: where to run jobs (Lustre), where not to (home/groups), and a link to purge policy.
Prompts

slurm-safety
Rendered from clusters.prompts.slurm_safety_template (e.g., “Always use -p/-t; cd to /lustre/groups/<group>/<user>; avoid login-node compute.”) This encodes Pegasus norms to keep agents from running heavy commands on log001–log004. [Pegasus access/policy reference] (hpc.gwu.edu)
Health tool (optional but recommended)

health.check (cheap readiness probe)
Input: cluster?: string
Output:
ok: true
master_state: "Running" | "Missing" | "StaleSocket" | "Needs2FA" | "Error"
control_path: string
message: string
Result and error examples

A) Needs2FA from shell.exec
{
"ok": false,
"code": "Needs2FA",
"message": "ControlMaster is not active for cluster 'pegasus'. Run 2FA to establish it.",
"details": {
"cluster": "pegasus",
"hint": "Run: gwu-hpc-mcp up --cluster pegasus --user rileyleff"
}
}

B) validate_slurm success (injection)
Input:
{ "command": "srun python train.py", "inject_defaults": true }

Output:
{
"ok": true,
"normalized_command": "srun -p defq -t 01:00:00 python train.py",
"partition_ok": true,
"time_ok": true,
"suggestions": ["Added default -p defq", "Added default -t 01:00:00"],
"warnings": []
}

C) session.exec final
{
"ok": true,
"exit_code": 0,
"duration_ms": 5420,
"streams": { "tmux": "...\nMCP_DONE:7f3e9c3d:0\n" },
"tail": "MCP_DONE:7f3e9c3d:0\n"
}

Mapping to the Rust SDK

Each tool is registered with a handler that:
Deserializes the input to a typed struct (serde).
Reads the current Config (Arc<RwLock<...>>).
Runs async work on tokio tasks; uses the MCP context to log output lines as they arrive.
Accumulates tail and status/warnings; returns the final JSON object.
Resources and prompts are registered at startup and re-registered on config reload. The server keeps a monotonically increasing config_version to let clients detect changes.
Policy alignment (Pegasus)

The surface enforces or guides the following site rules, set via TOML so they can be updated without rebuild:
Login nodes are for submission/data movement/compilation; do not run compute on them. [Pegasus access] (hpc.gwu.edu)
Slurm is required for compute; specify -p and -t; maximum 14 days. [Slurm usage] (hpc.gwu.edu)
Use Lustre scratch (/lustre, /lustre/groups) for active workloads; home/groups are NFS and not for running jobs. [Filesystems] (hpc.gwu.edu)
Next

If these tool/resource/prompt definitions and schemas look good, I’ll send 04-implementation-guide.md with crate skeletons, function signatures, and step-by-step tasks to build the binary.