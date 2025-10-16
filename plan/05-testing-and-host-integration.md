Document 05: 05-testing-and-host-integration.md

Purpose

Provide a complete strategy to verify gwu-hpc-mcp: unit tests, integration tests with a fake ssh/tmux harness, manual E2E on Pegasus, Codex CLI integration, CI, release checklist, and troubleshooting.
Test strategy (overview)

Unit tests: pure Rust logic (config parsing, time parsing, command building, Slurm validation, policy rendering, tail buffers).
Integration tests: spawn the server against fake ssh/tmux binaries to simulate ControlMaster checks, exec streaming, timeouts, and tmux behavior deterministically.
E2E (manual): real OpenSSH and tmux to Pegasus with 2FA; verify shell.exec and tmux session tools; confirm policy resources and validation tools.
Host integration: Codex CLI config snippet and validation flow.
CI: GitHub Actions on macOS and Linux with unit + integration (fake ssh); manual gate for real-cluster tests.
Release: build, sign (macOS optional), publish, and a smoketest checklist.
Unit tests (what to cover)

config
Parse a minimal TOML and the full Pegasus example.
Unknown major version returns a Config error.
Defaults applied as expected (pty, login_shell, default_cwd).
util
parse_slurm_time for: D-HH:MM:SS, HH:MM:SS, MM:SS; invalid strings return None.
duration_to_slurm round-trips typical values.
TailBuffer: push chunks beyond capacity and ensure tail keeps last N bytes.
quote_for_bash handles spaces, quotes, and dollar signs safely.
validate
Commands: “srun python x.py” (missing -p/-t), “srun -p defq -t 00:30:00 …”, “sbatch --partition=short --time=15:00”.
Unknown partition suggestions; time > max walltime produces a fix suggestion.
inject_defaults true inserts defaults and normalizes order.
policy
Render policy template with all placeholders; ensure variables resolve.
Render slurm-safety template; include scratch path/lustre roots.
ssh arg/builders (without running ssh)
build_remote_command uses cd && export K='V' && cmd and wraps with bash -lc when requested.
build_ssh_args toggles -tt vs -T correctly and injects ControlPath/port.
Integration tests (fake ssh/tmux harness)

Add a dev-only tiny helper binary in tests/bin (or a separate test crate) that mimics:
ssh -S … -O check: return code 0 with “Master running (pid=12345)” when a sentinel file exists; else nonzero.
ssh -S … -O exit: delete the sentinel; exit 0.
Exec mode: if argv contains “bash -lc”, print canned stdout in chunks with small delays to test streaming; optionally write to stderr for non-PTY paths.
Simulate a long command that times out (sleep) to verify SIGINT/kill path.
For tmux commands: new-session/list/capture-pane/display-message/send-keys operate on in-memory maps; MCP_DONE:<uuid>:<code> appears in the stream when send-keys “runs” a command.
Point the server at the fake binaries by overriding config.ssh.ssh_binary and ssh.tmux_binary in a test-only config.
Test cases:
ControlMaster stale socket: fake check returns nonzero with socket file present; server recognizes StaleSocket and reports Needs2FA to tool callers.
shell.exec PTY mode: merged “pty” stream; ensure chunking and tail are present in final result.
shell.exec non-PTY: separate stdout/stderr streams appear; exit_code captured.
Timeout: verify SIGINT then kill and final error object (code=Timeout).
session.open/exec/close: sentinel detection; streamed chunks; exit code propagation.
validate_slurm end-to-end: tool input/outputs match validator module.
E2E manual tests (real Pegasus)

Prereqs: OpenSSH client (ssh) and tmux installed locally; VPN active if off-campus; your TOML saved to ~/.gwu-hpc-mcp/config.toml with cluster id “pegasus” and your default_cwd (e.g., /lustre/groups/gedanlab/rileyleff).
Establish ControlMaster (interactive 2FA):
gwu-hpc-mcp up --cluster pegasus --user your_netid
Verify with: gwu-hpc-mcp status (should show Master running and the ControlPath).
Run server (stdio) for a quick MCP smoke via the Inspector (optional):
gwu-hpc-mcp serve-stdio
Use the MCP Inspector to connect and list tools/resources; run hpc.describe and fetch hpc://pegasus/policy.
Exec path:
shell.exec: command="hostname && echo $PWD && sinfo | head -n 5", cwd=<your Lustre path>, timeout_s=60
Expect streaming output; exit_code=0; tail populated.
Slurm validation:
hpc.validate_slurm: command="srun python train.py", inject_defaults=true
Confirm defaults inserted (partition/time) per TOML; try “-t 20-00:00:00” to see cap suggestion at 14-00:00:00.
Tmux session:
session.open: name="work1", cwd=<Lustre path>
session.exec: name="work1", command="module avail | head -n 20", timeout_s=30
session.close: name="work1"
Teardown:
gwu-hpc-mcp down
Codex CLI integration (stdio)

Install gwu-hpc-mcp in your PATH (see Release section).
Edit ~/.codex/config.toml:
startup_timeout_sec = 30
[mcp_servers.gwu_hpc]
command = "/absolute/path/to/gwu-hpc-mcp"
args = ["serve-stdio", "--config", "/Users/you/.gwu-hpc-mcp/config.toml"]
env = { GWU_HPC_MCP_USER = "your_netid" }
Restart Codex CLI; verify:
List MCP servers; gwu_hpc should show connected.
In a trusted project, run a simple shell.exec; if you see Needs2FA, run the “up” command as instructed, then retry.
CI (GitHub Actions)

Matrix: ubuntu-latest, macos-latest
Steps:
checkout
rustup toolchain stable with caching
cargo fmt --check; cargo clippy -- -D warnings
cargo test --all-features
Set env to use fake ssh/tmux for integration tests:
Run with GWU_HPC_MCP_TEST_FAKE=1
Provide test config file path via --config to integration tests or env
Artifacts: upload cargo-test logs on failure
Do not run E2E against Pegasus in CI (requires 2FA). Keep a separate manual job or GitHub Actions “workflow_dispatch” with secrets if you later add a non-interactive smoke.
Release and packaging

Build:
cargo build --release
Output binary at target/release/gwu-hpc-mcp
Optional:
macOS: codesign if distributing internally
Create tarballs: gwu-hpc-mcp-{os}-{arch}.tar.gz with SHA256 sums
Publish:
Attach binaries to a GitHub Release with the commit/tag
Include a sample config at examples/pegasus.config.toml
Install guide:
Place the binary in a directory on PATH (e.g., /usr/local/bin)
Ensure ssh and tmux are installed and discoverable (which ssh, which tmux)
Create ~/.gwu-hpc-mcp/config.toml and set your default_cwd and user
Troubleshooting (field guide)

Needs2FA or MasterDown:
Run: gwu-hpc-mcp up --cluster pegasus --user your_netid in a real terminal; complete 2FA; retry.
Stale ControlPath:
gwu-hpc-mcp down; if socket remains, manually rm the path printed by status; then up.
Consider using an underscore (_) before port in control_path to avoid colon issues.
PATH issues (host can’t spawn):
Codex logs show “command not found.” Use absolute path to gwu-hpc-mcp in config.
No streaming / early timeout:
Increase startup_timeout_sec in Codex config for first cold start.
Set RUST_LOG=gwu_hpc_mcp=info to see stderr logs.
ssh/tmux not found:
Install via OS package manager; adjust config.ssh.ssh_binary and ssh.tmux_binary if needed.
Safety tests and checks

Denylist:
Unit test: shell.exec of “rm -rf /” → ok=false, code="DeniedByPolicy".
compute_markers warning:
“python -m torchrun …” on login (no Slurm context) → emits a warning in status array and logs.
validate_slurm:
Missing partition/time returns clear suggestions; exceeding max returns a cap suggestion.
Performance checks (manual)

Cold start (served via Codex):
Process spawn to MCP handshake under ~300 ms (warm); under 2 s first-run.
Streaming latency:
command: seq 1 1000 with small sleeps; verify log chunks arrive continuously and final tail contains last lines.
Artifacts for testers

Example commands:
gwu-hpc-mcp up --cluster pegasus --user $USER
gwu-hpc-mcp serve-stdio
gwu-hpc-mcp status
gwu-hpc-mcp test
Example config path:
~/.gwu-hpc-mcp/config.toml
That’s the final document in the plan set. If you want, I can now generate the initial Rust crate skeleton (files and stubs matching this plan) so you can drop it into a repo and start iterating.