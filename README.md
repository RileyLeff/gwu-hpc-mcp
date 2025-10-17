# gwu-hpc-mcp

This is Riley's prototype tool for allowing LLM agents, such as OpenAI's [Codex CLI](https://developers.openai.com/codex/cli/) or Anthropic's [Claude Code](https://www.claude.com/product/claude-code), to interact with the High-Performance Computing resources at George Washington University.

## What It Can Do

Use natural language to instruct your LLM agent 

## How It Works

### For You, The End User

#### Initial Setup

Note that this server only works on unix or unix-like systems: Linux, MacOS, or WSL on Windows. This will not work in a native windows environment like Powershell.

1. Install the server in any one of the following ways: 

via cargo (available once I make this repo public): 
## Install Options

### Using Cargo (recommended)
```bash
cargo install --path .
# or once the repo is public:
# cargo install --git https://github.com/<org>/gwu-hpc-mcp
```

### Build from source
```bash
cargo build --release
cp target/release/gwu-hpc-mcp /usr/local/bin/
```

### Prebuilt binaries
Download the latest release artifact and place it on your `$PATH` (to be provided once CI publishes archives).

### Initialize the config
```bash
# scaffold with defaults and your scratch path
gwu-hpc-mcp config init --default-cwd /scratch/groups/<group>/<user>
# override output location or overwrite an existing file if needed
gwu-hpc-mcp config init --default-cwd /scratch/groups/<group>/<user> --output ~/.gwu-hpc-mcp/config.toml --force
```

Alternatively, copy the example:
```bash
mkdir -p ~/.gwu-hpc-mcp
cp examples/pegasus.config.toml ~/.gwu-hpc-mcp/config.toml
# then edit ~/.gwu-hpc-mcp/config.toml manually
```

## FAQ

- *What is MCP?*
  - [**M**odel **C**ontext **P**rotocol](https://modelcontextprotocol.io/docs/getting-started/intro) is a standard for exposing external tools to language models -- a MCP server's job is to accept tokens (words, text, etc) from a language model and convert them into actions or tool use. You can find many MCP servers for many different applications [here](https://www.claude.com/product/claude-code).
- *Why not just use an off-the-shelf SSH MCP tool?*
  - GWU's HPC has a 2FA step that was very awkward to handle with the SSH MCP tools I tried. This is a simple attempt at a more suitable tool for this particular job.
- *Can I use this with Github Copilot, Qwen Code, Opencode, Gemini CLI, Amp, Roo, or whatever other agent I'm using?*
  - Yes, as long as your agent supports MCP, which is fairly standard across agent implementations at this point.
- *Why is this written in rust? Most MCP servers I see elsewhere are written in typescript or python.*
  - I got very frustrated with managing the statefulness of the ssh connection + a runtime. Using the [MCP Rust SDK](https://github.com/modelcontextprotocol/rust-sdk) was a much more pleasant experience.



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
When installed via Cargo, the crate also ships a short alias `ghm`, so you can swap `command = "ghm"` if you prefer the shorthand.

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
