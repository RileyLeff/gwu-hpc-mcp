# Connection instructions for agents

Use the GWU Pegasus MCP tools for cluster work. The MCP runs locally and reuses
an SSH connection established by the human with `ghm up`. SSH calls this shared
connection a ControlMaster; explain it to users as the "cluster connection".
It is not a second access route or an authentication bypass.

Before remote work, call `health_check` and read `hpc_describe`. A Running health
check confirms the local SSH master, not end-to-end reachability; a harmless
`shell_exec` such as `hostname` can verify the remote connection.

Off campus, the user must connect to GW's VPN and keep it connected. Losing VPN
interrupts cluster access and can terminate the SSH connection. Reconnecting VPN
alone may not restore the authenticated connection.

If the connection is missing, stale, or drops during a command:

1. Tell the user that cluster access is unavailable. Do not claim an interrupted
   command succeeded or assume a submitted job stopped.
2. Ask them to check their VPN connection if off campus, then run
   `ghm up --user THEIR_NETID` in their own terminal (include the configured
   `--cluster` or `--config` when needed) and complete authentication themselves.
3. Never guess, generate, reuse, or submit passwords or 2FA/verification codes.
   Do not ask the user to paste credentials or codes into chat. Do not launch an
   interactive authentication loop, bypass MFA, or fall back to another SSH route.
4. Do not run `ghm down`, kill SSH processes, or delete sockets automatically:
   another agent or terminal may be using the shared connection. If recovery
   needs a reset, explain the impact and coordinate with the user first.
5. After the user confirms reconnection, recheck health and run a harmless
   command. Inspect Slurm job state before retrying a submission whose outcome
   was uncertain; avoid submitting duplicate jobs.

Multiple MCP clients can share the cluster connection. Use distinct tmux session
names per task. Do not drive, close, or interrupt another client's sessions.
`shell_exec` runs without a PTY by default; pass `pty=true` only for programs that
need a terminal. Pass explicit timeouts for anything longer than two minutes.
Use the configured scratch directory and Slurm for compute; inspect live resource
metadata before choosing a partition or GPU type. See `hpc_describe` for policies.
