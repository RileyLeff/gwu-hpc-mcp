# Register ghm with Claude Code or OpenAI Codex

These are two different clients: **Claude Code** (Anthropic) and **Codex**
(OpenAI). Install `ghm` once, then register it separately in each client you use.
This guide assumes that client is already installed and signed in.

First follow the [platform installation and first connection guide](../README.md).
Connect GW VPN off campus, initialize your config, and complete `ghm up --user
YOUR_NETID` in your terminal. Never put a password or verification code in chat or
in MCP configuration. Use `ghm --version` to check the binary actually on PATH;
old chat transcripts may refer to older installed versions.

## Claude Code: user scope (all projects)

In a macOS/Linux/WSL shell, replace `YOUR_NETID`:

```sh
claude mcp add --transport stdio --scope user gwu_hpc -- \
  "$HOME/.cargo/bin/ghm" serve-stdio \
  --config "$HOME/.gwu-hpc-mcp/config.toml" --user YOUR_NETID
claude mcp get gwu_hpc
claude mcp list
```

User scope is stored in `~/.claude.json`. Use the command instead of overwriting
that file by hand. If `gwu_hpc` already exists, inspect it with `claude mcp get`
and update that entry rather than adding duplicates or deleting unrelated servers.
For a deliberately project-shared setup, Claude supports `--scope project`, but
do not commit your personal paths or account settings to a shared project.

In the running Claude Code session, open `/mcp` and check/reconnect `gwu_hpc`.
This was enough to expose the tools in the maintainer's live Claude test. If your
client version does not pick them up, exit and relaunch Claude Code. Do not
restart the cluster SSH connection simply to refresh the MCP tool list.

Connected means the local MCP server answered. Verify cluster access separately
with the acceptance checks below. See the
[official Claude Code MCP documentation](https://code.claude.com/docs/en/mcp).

## OpenAI Codex: user configuration

In a macOS/Linux/WSL shell, replace `YOUR_NETID`:

```sh
codex mcp add gwu_hpc -- "$HOME/.cargo/bin/ghm" serve-stdio \
  --config "$HOME/.gwu-hpc-mcp/config.toml" --user YOUR_NETID
codex mcp get gwu_hpc
codex mcp list
```

Alternatively, merge this into your user `~/.codex/config.toml`, replacing paths
with absolute paths from your machine. Choose either CLI registration or this
manual method; do not add a second table with the same name.

```toml
[mcp_servers.gwu_hpc]
command = "/ABSOLUTE/PATH/TO/.cargo/bin/ghm"
args = ["serve-stdio", "--config", "/ABSOLUTE/PATH/TO/.gwu-hpc-mcp/config.toml", "--user", "YOUR_NETID"]
```

For the desktop app, edit the user configuration on the host where the app runs;
registration in a different WSL account does not edit the Windows host's config.
Reconnect the MCP integration or restart the Codex session/app if it still shows
the old tool list or server instructions. You do not need to rerun `ghm up` when
its SSH connection is still healthy. `codex mcp login` is for MCP OAuth servers;
it is not the authentication step for this local SSH-backed server.

For longer tool calls, check the client's tool timeout as well as the MCP
`timeout_s` argument. Run durable compute as Slurm jobs and poll their status.
See the [official Codex MCP documentation](https://developers.openai.com/codex/mcp).

## Windows-native clients: launch the server inside WSL

Use the same WSL distribution and Linux account where you installed `ghm` and
ran `ghm up`. Replace both `YOUR_WSL_USER` and `YOUR_NETID`; they are different
identities. In PowerShell, register the client you use:

```powershell
claude mcp add --transport stdio --scope user gwu_hpc -- wsl.exe --distribution Ubuntu --user YOUR_WSL_USER --exec /home/YOUR_WSL_USER/.cargo/bin/ghm serve-stdio --config /home/YOUR_WSL_USER/.gwu-hpc-mcp/config.toml --user YOUR_NETID
```

```powershell
codex mcp add gwu_hpc -- wsl.exe --distribution Ubuntu --user YOUR_WSL_USER --exec /home/YOUR_WSL_USER/.cargo/bin/ghm serve-stdio --config /home/YOUR_WSL_USER/.gwu-hpc-mcp/config.toml --user YOUR_NETID
```

For Codex desktop without the CLI installed, the equivalent host-side TOML is:

```toml
[mcp_servers.gwu_hpc]
command = "wsl.exe"
args = ["--distribution", "Ubuntu", "--user", "YOUR_WSL_USER", "--exec", "/home/YOUR_WSL_USER/.cargo/bin/ghm", "serve-stdio", "--config", "/home/YOUR_WSL_USER/.gwu-hpc-mcp/config.toml", "--user", "YOUR_NETID"]
```

These Windows launch recipes have not been tested on a Windows host here. For
clients already running inside WSL, use the ordinary Linux commands above.
Native Windows execution of ghm without WSL is not supported.

## Instructions you can give an agent doing setup

> Read this repository's README and docs/agent-connection.md. Install ghm locally
> and configure my verified scratch directory and NetID. Register gwu_hpc in my
> chosen client's user configuration using docs/client-setup.md, preserving other
> entries. Have me connect GW VPN and run ghm up in my own terminal for 2FA; never
> ask me to paste a code or attempt authentication for me. Verify the registered
> command and explain whether I need to reconnect MCP or restart the client.

## Acceptance checks after registration

Ask your agent to:

1. Call `health_check` and confirm Running.
2. Read `hpc_describe` and its policy guidance.
3. Run `hostname`, `id -un`, and `pwd` through `shell_exec` with `pty=false` and a
   short timeout. Confirm the intended account and directory.
4. Optionally open a uniquely named temporary tmux session, print a short test
   message with `session_exec`, then close only that test session.

No compute job is needed for these checks. Claude Code and Codex may share the
same ghm SSH connection. Use separate tmux sessions and coordinate before
resetting SSH. The user must restore VPN and complete any new 2FA if it drops.

## CLI use without client registration

Humans and shell-capable agents can run these directly:

```sh
ghm --help
ghm status --user YOUR_NETID
ghm doctor --user YOUR_NETID
ghm test --user YOUR_NETID
```

`status` checks the local SSH master; `doctor` checks configuration and local SSH
availability; `test` runs the fixed remote command `hostname && pwd`. `config init`
sets up the config. The human runs `up` for authentication. `down` closes the
shared connection and must be coordinated with other users/agents.

There is currently **no `ghm exec` or CLI equivalent for every MCP tool**.
Arbitrary remote commands, policy resources, and tmux operations are exposed by
MCP. Prefer registering the server for normal agent work.

A script can instead launch `ghm serve-stdio` as a subprocess and perform the MCP
JSON-RPC initialization and tool calls itself. That uses the real MCP protocol
without registering it in Claude/Codex; it is not bypassing MCP. The repository's
`tests/mcp_smoke.py` demonstrates this and cleans up its own test session.
Simply launching `serve-stdio` in a terminal does not provide an interactive shell.

Direct `ssh` with the correct socket is technically possible, but is a separate
low-level SSH operation that omits ghm's MCP responses and policy guidance. Do not
use another SSH route to work around missing authentication. The same VPN/2FA
and cluster policies apply regardless of which client initiates commands.
