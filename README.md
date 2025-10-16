# gwu-hpc-mcp

## Overview
- Exposes the George Washington University (GWU) Pegasus HPC cluster to MCP-compatible hosts (e.g., Codex CLI) over stdio.
- Provides high-fidelity remote shell execution and tmux-backed persistent sessions via a user-managed OpenSSH ControlMaster.
- Streams stdout/stderr in near real time and surfaces cluster policy and filesystem guidance as MCP resources/prompts.
- Loads live cluster metadata from a TOML configuration file; changes are picked up at runtime via a filesystem watcher.

## Components
- `src/main.rs`: CLI entrypoint with subcommands for establishing/tearing down the ControlMaster, status checks, tests, diagnostics, and the default `serve-stdio`.
- `src/mcp.rs`: MCP server wiring using the `rmcp` SDK. Implements tools (`shell_exec`, `session_open/exec/close`, `hpc_describe`, `health_check`) plus resources and prompts.
- `src/ssh.rs`: ControlMaster state management, remote execution with timeouts/cancellation, stream handling, and tmux command support.
- `src/sessions.rs`: High-level tmux management (sentinel-based command execution, streaming output).
- `src/config.rs`: Configuration schema, loader, hot-reload-aware manager, and regex compilation.
- `src/policy.rs`: Renders policy/filesystem/partition resources and prompts from templates.
- `src/util.rs`: Shared helpers (tail buffer, path expansion, shell escaping).
- `plan/`: Original planning documents kept for reference.

## Getting Started
### Prerequisites
- Rust 1.75+ (the project targets edition 2024).
- OpenSSH and tmux installed locally.
- Access to GWU Pegasus with valid credentials and the ability to complete the 2FA challenge.

### Build & Test
```bash
cargo fmt
cargo test
```
The test suite currently includes focused unit tests for configuration parsing, template rendering, tail buffer behavior, and URI parsing.

### Configuration
1. Copy the Pegasus example from `plan/01-config-and-cluster-metadata.md` (or author your own) into `~/.gwu-hpc-mcp/config.toml`.
2. Adjust `defaults.default_cwd`, filesystem prompts, and policy text to match your group/project paths.
3. Optional: set `GWU_HPC_MCP_CONFIG` to point to a non-default location.

Key fields:
- `[[clusters]]`: identifies the cluster (`id`, `display_name`, `host`, `port`).
- `[clusters.ssh]`: ControlMaster `control_path`, `ssh_binary`, `tmux_binary`, keepalive settings.
- `[clusters.filesystems]` & `[clusters.policy]`: variables used by resource/prompt templates.
- `[defaults]`: controls login shell usage, PTY, concurrency, default cluster/cwd.

### Establishing the ControlMaster
```
gwu-hpc-mcp up --cluster pegasus --user <netid>
```
- Runs `ssh -MNf ...` with the configured ControlPath. Interactive 2FA occurs here; the MCP server never prompts for credentials.

### Running the MCP Server
```
gwu-hpc-mcp serve-stdio --config ~/.gwu-hpc-mcp/config.toml
```
- The process listens on stdio using the `rmcp` transport. Logs go to stderr (respecting `RUST_LOG`).
- Configuration changes are reloaded automatically (notify watcher).

### CLI Utilities
- `gwu-hpc-mcp status` — shows ControlMaster state and socket path.
- `gwu-hpc-mcp test` — runs `hostname && pwd` via the ControlMaster and prints output.
- `gwu-hpc-mcp down` — shuts down the ControlMaster and removes the socket.
- `gwu-hpc-mcp doctor` — diagnostic checks (binaries, ControlPath, Master state).

### Codex CLI Integration (example)
Add to `~/.codex/config.toml`:
```toml
startup_timeout_sec = 30

[mcp_servers.gwu_hpc]
command = "/absolute/path/to/gwu-hpc-mcp"
args = ["serve-stdio", "--config", "/Users/you/.gwu-hpc-mcp/config.toml"]
env = { GWU_HPC_MCP_USER = "your_netid" }
```
Steps:
1. Ensure `gwu-hpc-mcp` is on your `$PATH` and the ControlMaster is up.
2. Restart Codex CLI; the server should appear under available MCP servers.
3. Use standard MCP commands (`shell_exec`, `session_*`, `hpc_describe`, `health_check`) from the agent environment. It’s best practice to fetch `hpc://pegasus/policy` (or call `hpc_describe`) at the start of a session so the agent has the latest site tips before running heavy commands.

## Divergence from the Original Plan
### 1. Slurm Validation Tool
- **Original**: Provide an `hpc.validate_slurm` tool to parse Slurm commands, enforce partition/time requirements, and suggest fixes.
- **Implemented**: Omitted `hpc.validate_slurm`. The server exposes policy guidance via prompts/resources only, relying on HPC enforcement (per user preference). No job-submission wrappers or command validation occur on the automation side.

### 2. Prompt/Resource Behavior
- **Original**: Intended both prompts/resources for Slurm policy plus optional machine-friendly partitions resource.
- **Implemented**: Prompts/resources remain, but validation logic is absent. The resources are rendered plain-text; no additional machine JSON schema was introduced.

### 3. Optional Features
- Included optional features in the first pass as requested:
  - **Config Hot Reload** using `notify`.
  - **`health.check` Tool** that reports ControlMaster status and socket path.
  - Tail buffer warnings, denylist, and compute markers as described.

### 4. MCP Toolkit
- Maintained strict adherence to stdio-only transport and kept logs on stderr per plan.
- No extra tools beyond the shell/tmux/describe/health surfaces; ensured streaming output via logging notifications.

### 5. Testing Scope
- Added focused unit tests (tail buffer, policy templating, config schema, URI parsing) rather than full fake-SSH integration harness (left for future iterations). Unit coverage ensures critical helpers behave, but no end-to-end fake ssh/tmux simulation yet.

## Future Enhancements
- Expand test coverage with a fake SSH/tmux harness to simulate ControlMaster behavior without external dependencies.
- Add optional machine-readable resources (e.g., JSON partitions list) if agents benefit from structured data.
- Integrate richer logging/telemetry or metrics depending on deployment needs.

## Module Search Tips (mirrors policy prompt)
- Keep `login_shell` enabled unless you have a specific reason to run a raw command; it ensures the environment module system works.
- Use `module avail | grep <term>` to trim long lists, or pipe to `head`/`less`.
- `module spider <term>` reveals availability and prerequisites; `module keyword <term>` searches descriptions.
- These hints are baked into the policy/prompt templates so agents see them automatically, but they’re helpful for humans working interactively too.
