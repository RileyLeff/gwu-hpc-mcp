Document 00: 00-index.md
Title: gwu-hpc-mcp — plan overview

What this is

A compact, end-to-end plan to implement gwu-hpc-mcp: a single Rust binary that exposes GWU’s Pegasus HPC cluster to local LLM agents via MCP (stdio). The server provides high-fidelity shell execution (via OpenSSH ControlMaster), optional tmux-backed sessions, streaming IO, and runtime-configurable cluster knowledge (partitions, policies, filesystem guidance) loaded from TOML.
Why this exists

Pegasus requires VPN/2FA and Slurm-only compute; running on login nodes is prohibited. Users should use Slurm partitions and set a time limit; the max job run time is 14 days. Scratch is the Lustre filesystem (/lustre), while home and groups are NFS and not intended for running jobs. These policies inform default behavior and validation. (hpc.gwu.edu)
What you build

A Rust MCP stdio server using the official MCP Rust SDK (rmcp crate). This gives fast startup and robust async handling with tokio. (github.com)
One binary that:
Manages an OpenSSH ControlMaster lifecycle safely (detects stale sockets, verifies, recovers).
Executes remote commands with PTY and login shells, streams output, supports timeouts/cancel.
Offers tmux sessions (open/exec/close) with sentinel-based exit detection and streaming.
Loads cluster metadata and policy at runtime from a TOML file; renders resources/prompts and provides structured tools (e.g., describe, validate_slurm).
Can be spawned by OpenAI Codex CLI as a stdio MCP server.
Deliverables

Source tree with a Cargo workspace (single binary crate is fine initially).
A sample TOML config for Pegasus with:
host/port, login_nodes, filesystem info (lustre root, scratch defaults), Slurm partitions, max walltime, and policy/prompt templates. (hpc.gwu.edu)
MCP surface: tools (shell.exec, session.open/exec/close, hpc.describe, hpc.validate_slurm), resources (hpc://pegasus/policy, hpc://pegasus/partitions, hpc://pegasus/filesystem), prompts (slurm-safety).
Tests: unit + integration (fake ssh), an e2e script, and a “doctor” command for local checks.
Codex CLI integration snippet for ~/.codex/config.toml.
How to use this plan

Read 01-config-and-cluster-metadata.md to finalize the TOML schema and fill the Pegasus example from GWU docs.
Read 02-architecture-and-state.md to align on the Rust process model, ControlMaster lifecycle, and tmux streaming.
Read 03-mcp-surface-and-schemas.md to lock the MCP API (tools/resources/prompts) and JSON IO.
Implement following 04-implementation-guide.md (crate skeletons, tasks, error handling).
Validate via 05-testing-and-host-integration.md (fake ssh, real ssh dry-runs, Codex).
Key references (Pegasus)

Connecting and login nodes: pegasus.arc.gwu.edu; log001–log004; VPN/off-campus access. (hpc.gwu.edu)
Slurm partitions, required -p and -t, 14-day max. (hpc.gwu.edu)
Filesystems, use /lustre for active workloads; home/groups are NFS and not for running jobs; purge policy on Lustre. (hpc.gwu.edu)
Pegasus overview (login nodes, GPU/CPU node types). (hpc.gwu.edu)
MCP Rust SDK

modelcontextprotocol/rust-sdk (rmcp crate, server feature). Use a recent released version (e.g., 0.6.x) or the main branch if you need fixes. (github.com)
Assumptions

Local OpenSSH client is available.
User can complete one-time 2FA in their terminal when establishing the ControlMaster.
Codex CLI (or similar MCP host) will spawn the binary over stdio with an increased startup timeout if needed.
Success criteria

Cold start under ~300 ms (warm) and <2 s (first run), handshake reliable with Codex.
shell.exec and tmux session tools stream with low latency; validate_slurm gives actionable fixes.
Updating the TOML (e.g., partitions or prompts) reflects immediately after a reload.
