# gwu-hpc-mcp

Let your AI assistant use GWU Pegasus through your own authenticated cluster
connection. You complete 2FA in your terminal; the agent never needs your password
or verification code. `ghm` is the short name for `gwu-hpc-mcp`.

## Before you start

- You need a GW Pegasus account and your GW NetID (which may differ from your
  computer username).
- **Off campus, connect to [GW VPN](https://it.gwu.edu/vpn-global-protect) first
  and keep it connected. Losing VPN interrupts cluster access and can drop the
  SSH connection.** Reconnect VPN and reauthenticate if necessary.
- You need an MCP-capable agent/client that can launch a local stdio server.
- macOS and Linux are supported. On Windows, use WSL; native Windows OpenSSH is
  not the supported connection-sharing route for this tool.
- The cluster needs Bash, tmux, and GNU `timeout`. These were available in the
  live Pegasus smoke tests. They do not need to be installed locally.

## Install on your computer

The current distribution is source-based. The GitHub repository is private at
this writing: request repository access from the maintainer or obtain a source
archive containing these updated files. A private repository requires GitHub
authentication to clone. There are no packaged installers or prebuilt release
binaries provided by these instructions.

### macOS

In Terminal, install Apple's command-line build tools if not already installed:

```sh
xcode-select --install
```

Finish the installer, then install current stable Rust using the
[official Rust installer](https://rust-lang.org/tools/install/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

OpenSSH ships with macOS. Continue to **Build and install** below.

### Linux

On Ubuntu/Debian, install the build tools and SSH client:

```sh
sudo apt update
sudo apt install build-essential curl git openssh-client
```

On Fedora use `sudo dnf install gcc make curl git openssh-clients`; on other
Linux distributions install the equivalent packages. Install current stable
Rust using the two Rust installer commands above. Then continue below.

### Windows (WSL)

In an administrator PowerShell window, follow
[Microsoft's WSL installation guide](https://learn.microsoft.com/en-us/windows/wsl/install):

```powershell
wsl --install -d Ubuntu
```

Restart if prompted, open Ubuntu, and finish creating your Linux user. Run the
Linux instructions above **inside Ubuntu**. Keep the repository, config, SSH
socket, and `ghm up` in the same WSL distribution and Linux user account. Native
Windows and WSL home directories and SSH sockets are not interchangeable.

Connect GW VPN on Windows and verify that Pegasus is reachable from WSL. If
Windows can reach it but WSL cannot, resolve the WSL/VPN networking issue before
troubleshooting MCP. Do not disable VPN or authentication to work around it.

### Build and install

Once you have access to the repository:

```sh
git clone https://github.com/RileyLeff/gwu-hpc-mcp.git
cd gwu-hpc-mcp
cargo install --path . --locked
ghm --version
```

If you received a source archive, extract it and run `cargo install --path .
--locked` from the folder containing `Cargo.toml` instead of cloning. Use a
current stable Rust toolchain; the source requires Rust 1.88 or newer. If Rust
is already installed, `rustup update stable` updates it.

Both `ghm` and `gwu-hpc-mcp` are installed in `~/.cargo/bin`. If your terminal
cannot find them, run `. "$HOME/.cargo/env"` or reopen the terminal. A GUI agent
may need the absolute executable path even when your terminal can find `ghm`.

## First connection: from config to ghm up

Replace `YOUR_NETID`, `YOUR_GROUP`, and `YOUR_USER` below. The scratch directory
must actually exist and be writable by you; ask your lab/HPC administrator if
you do not know it. Do not infer a filesystem directory from your group name.

```sh
ghm config init --default-cwd '/scratch/YOUR_GROUP/YOUR_USER'
ghm up --user YOUR_NETID
ghm status --user YOUR_NETID
ghm test --user YOUR_NETID
```

Keep VPN connected off campus. `ghm up` prompts interactively for the required
credentials/2FA. **Enter them only in your terminal, not in agent chat.** Check
that `ghm status` says `running` and `ghm test` prints a cluster hostname and exits
successfully. A first SSH connection may ask you to verify the host fingerprint;
use GW's trusted guidance rather than disabling host-key checking.

No edits to `~/.ssh/config` are required. You can optionally set
`export GWU_HPC_MCP_USER=YOUR_NETID` in your shell startup file to omit `--user`.
Still set the NetID explicitly in the agent configuration below, since GUI apps
may not inherit shell variables.

The config is `~/.gwu-hpc-mcp/config.toml`. The initializer uses the same template
as [example/pegasus.config.toml](example/pegasus.config.toml). For manual setup,
copy that example and add your real `default_cwd` under `[defaults]`. This excerpt
shows the key settings; it is not a replacement for the complete example:

```toml
[defaults]
default_cluster = "pegasus"
default_cwd = "/scratch/YOUR_GROUP/YOUR_USER"
login_shell = true

# In the Pegasus cluster's SSH section:
[clusters.ssh]
control_path = "~/.gwu-hpc-mcp/cm-%r@%h_%p"
```

Username precedence is `--user`, `GWU_HPC_MCP_USER`, then local `USER`. Config path
precedence is `--config`, `GWU_HPC_MCP_CONFIG`, then the default above. Use the same
NetID and config for the terminal connection and the MCP process. `config init
--force` replaces an existing config; do not use it to casually upgrade a config
that contains your settings.

## Connect your agent

For copy-paste registration commands, user/global scope, reconnect instructions,
and agent-led setup, see [Claude Code and OpenAI Codex setup](docs/client-setup.md).
It includes Windows-native clients launching ghm through WSL.


For clients using the common JSON `mcpServers` format, merge this into the
client's MCP configuration (do not replace unrelated server entries):

```json
{
  "mcpServers": {
    "gwu_hpc": {
      "command": "/ABSOLUTE/PATH/TO/ghm",
      "args": ["serve-stdio", "--user", "YOUR_NETID"],
      "env": {
        "GWU_HPC_MCP_CONFIG": "/ABSOLUTE/PATH/TO/.gwu-hpc-mcp/config.toml"
      }
    }
  }
}
```

Use `command -v ghm` for the executable path and `echo "$HOME"` for your home
path. Examples: `/Users/YOUR_MAC_USER/.cargo/bin/ghm` on macOS and
`/home/YOUR_LINUX_USER/.cargo/bin/ghm` on Linux/WSL. Do not put literal `~` in a
client's executable path or JSON path value. Your client may use another config
format; preserve its required structure and use the same command/arguments.

For a **Windows-native client launching the WSL installation**, use an explicit
WSL distribution and Linux user. The NetID is a separate value:

```json
{
  "mcpServers": {
    "gwu_hpc": {
      "command": "wsl.exe",
      "args": [
        "--distribution", "Ubuntu",
        "--user", "YOUR_WSL_USER",
        "--exec", "/home/YOUR_WSL_USER/.cargo/bin/ghm",
        "serve-stdio",
        "--config", "/home/YOUR_WSL_USER/.gwu-hpc-mcp/config.toml",
        "--user", "YOUR_NETID"
      ]
    }
  }
}
```

Run `ghm up` in that same Ubuntu user account. For an agent already running
inside WSL, use the ordinary Linux example instead. Windows-client WSL launch
instructions have been documented but have not been exercised on a Windows host
in this project's current validation.

Reload/reconnect the MCP server in your client after configuring it. Ask your
agent to call `health_check`, then `hpc_describe`, then run `hostname` through
`shell_exec`. MCP startup alone does not prove that cluster access works.

## What is ControlMaster? Is it different from MCP?

They are two layers of the same workflow:

```text
You run ghm up -> SSH login + 2FA -> shared SSH connection to Pegasus
Agent -> local ghm serve-stdio (MCP) -> reuses that SSH connection -> Pegasus
```

OpenSSH calls connection sharing **ControlMaster**. The **ControlPath** is a
local socket through which other SSH commands reuse the authenticated connection.
`ghm up` supplies these SSH options directly, using the socket path in the ghm
config. See the [OpenSSH connection-sharing documentation](https://man.openbsd.org/ssh_config#ControlMaster).

An existing `Host pegasus` alias may have its own ControlMaster/ControlPath. A
working `ssh pegasus` session does not necessarily establish the socket that ghm
expects. Use `ghm up` and `ghm status` for this tool; do not copy someone else's
SSH config. The MCP is not a separate cluster login or an alternative to VPN/2FA.

Several MCP clients can share one ghm connection. Give each task its own tmux
session. `ghm down` affects every client sharing that connection, so coordinate
before disconnecting or resetting it.

## When the connection drops

1. Restore GW VPN if off campus. Network changes, laptop sleep, VPN loss, and
   SSH disconnection can interrupt access. A brief interruption may recover, but
   do not assume it did.
2. Run `ghm status --user YOUR_NETID`. A running local master is not a guarantee
   of remote reachability; also run `ghm test --user YOUR_NETID`.
3. If a new login is needed, run `ghm up --user YOUR_NETID` and complete 2FA in
   your terminal. If an earlier `ghm up` is stuck in that terminal, cancel that
   attempt with Ctrl-C before retrying.
4. If a stale connection requires resetting, first coordinate with other clients;
   then use `ghm down --user YOUR_NETID` followed by `ghm up --user YOUR_NETID`.
   If reset fails, inspect `ghm doctor` rather than deleting arbitrary sockets or
   killing all SSH processes.
5. Recheck health and a harmless command. Before retrying a job submission that
   lost its response, check `squeue`/`sacct` to avoid duplicate jobs. A lost SSH
   connection does not imply that Slurm jobs were cancelled.

[Agent connection instructions](docs/agent-connection.md) are embedded directly
in the MCP initialization response, so agents receive the VPN, reconnection,
no-code-guessing, and shared-session rules even if they never read this README.
The policy returned by `hpc_describe` repeats the authentication guidance.

## CLI use without MCP registration

Humans and agents can run `ghm status`, `ghm doctor`, and `ghm test` directly from
a shell. `test` runs the fixed `hostname && pwd` check; there is no general-purpose
`ghm exec` command. For arbitrary commands and tmux tools, register MCP or use a
script that speaks MCP to `ghm serve-stdio`. See the
[CLI versus MCP explanation](docs/client-setup.md#cli-use-without-client-registration).

## Tools and behavior

- `hpc_describe`: configured policy, filesystem, and partition snapshot. Read it
  before using the cluster; use `sinfo`/`scontrol` to verify current resources.
- `health_check`: reports whether the local SSH ControlMaster is running.
- `shell_exec`: executes a shell command, optionally with cwd, environment,
  timeout, and PTY. `pty=false` keeps stdout and stderr separate. `ok` is true
  only when `exit_code` is zero. A failed cwd stops execution.
- `session_open`, `session_exec`, `session_close`: tmux-backed execution.
  Each command runs in a fresh login Bash child in the session; shell variable
  and cwd changes do not persist between calls. Pass `cwd` and `env` explicitly.
  Output is buffered in private remote temporary files and returned on completion,
  independent of pane scrolling, terminal clearing, or Unicode. A session command
  defaults to a 300-second timeout; GNU `timeout` terminates its process group.
  Exit codes 124/137 are reserved for reporting timeout. Successful collection
  removes temporary files; connection failures may leave diagnostic files under
  `/tmp/gwu-hpc-mcp-*`. Commands to the same session are serialized within one
  server process; do not drive the same pane from multiple clients simultaneously.

The shell denylist is a heuristic, not a sandbox. Slurm guidance is advisory;
there is no job-submission wrapper enforcing resource or filesystem policy.
Commands run with your account's privileges. A shell timeout stops the local SSH
client; it does not cancel submitted Slurm jobs. Use `scancel` for those jobs.

Logs go to stderr, leaving stdout exclusively for MCP JSON. Config edits are
hot-reloaded when detected; concurrency changes require restarting the server.

## Cluster metadata

The bundled snapshot was checked against live Slurm on 2026-09-22. Current
partitions are `cpu`, `gpu`, `nano`, `superChip`, `viz`, and `deus`. Limits vary by
partition. GPU queues advertise GRES; inspect type/count and request what the job
needs. Old `defq` and `debug-gpu` examples are obsolete. CPU/GPU partitions contain
mixed hardware, so the config avoids claiming uniform specifications.

`/scratch` currently resolves under GPFS. The existing config field `lustre_root`
is retained as a compatibility name, not a claim about filesystem type. Confirm
retention and account eligibility with HPC staff. Use your own scratch directory;
never copy another user's local config unchanged.

## Development and validation

```sh
cargo fmt --check
cargo test --locked
cargo build --locked
python3 tests/mcp_smoke.py --binary target/debug/ghm
```

The smoke test uses the real MCP stdio protocol and requires an existing Pegasus
ControlMaster plus your local config. It creates and removes a uniquely named
tmux session, checks shell errors, Unicode/large output, invalid cwd, timeout,
and session reuse. It does not submit Slurm jobs.

`ghm doctor` checks local configuration and binaries. `ghm down` closes the SSH
ControlMaster. Keep the connection open while using the tools.
