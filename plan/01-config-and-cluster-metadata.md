Purpose

Define a single runtime TOML file that declares one or more HPC clusters, their SSH/Slurm/filesystem facts, and the policy/prompt text the server exposes to the LLM.
The Rust server loads this file at startup (and supports live reload), renders resources/prompts from templates, and uses the structured metadata for validation (e.g., require -p and -t, cap walltime, known partitions).
What must be captured (Pegasus-backed defaults)

Login and access:
Cluster entry point: pegasus.arc.gwu.edu. Individual login nodes: log001–log004. VPN or on-campus network may be required. Running jobs on login nodes is prohibited. (hpc.gwu.edu)
Slurm policy:
Scheduler: Slurm. Jobs must include both a partition (-p) and a time limit (-t) or they’re rejected; maximum walltime is 14 days. Partitions include CPU (defq, short, tiny, nano, highThru, highMem, debug-cpu) and GPU (small-gpu, large-gpu, debug-gpu). (hpc.gwu.edu)
Filesystems:
High-speed scratch: Lustre under /lustre (Pegasus groups under /lustre/groups). Home and groups are NFS; don’t run jobs there. Lustre is purgeable scratch (not archival). (hpc.gwu.edu)
Optional transfer info:
Globus “Pegasus HPC Cluster” endpoint exposes /lustre/groups; Box connector available. (hpc.gwu.edu)
TOML schema (v1)

Single file; default path: ~/.gwu-hpc-mcp/config.toml (overridable via env GWU_HPC_MCP_CONFIG).
Supports multiple clusters. Keys are stable, additive, and designed for straightforward serde deserialization.
Schema (authoritative)

top-level
version = 1

[defaults]
pty = true
login_shell = true
max_concurrent_execs = 4
warn_on_login_compute = true

Optional: default cluster to target if client omits it
default_cluster = "pegasus"

Where to place long-lived sessions by default (can be overridden per call)
default_cwd = "/lustre/groups/<your_group>/<your_user>"

[heuristics]
denylist = ["rm -rf /", "mkfs", "shutdown", "reboot"]

Rust will compile these as regexes; keep them conservative
compute_markers = [
"CUDA_VISIBLE_DEVICES",
"(?i)python.(torch|tensorflow|jax)",
"\bmpirun\b",
"\bsrun\b(?!.--pty)"
]

[[clusters]]
id = "pegasus"
display_name = "GWU Pegasus"
host = "pegasus.arc.gwu.edu"
port = 22

[clusters.ssh]

ControlMaster socket template; %r user, %h host, %p port
control_path = "~/.gwu-hpc-mcp/cm-%r@%h_%p"
server_alive_interval = 60
server_alive_count_max = 3
ssh_binary = "ssh"
tmux_binary = "tmux"

[clusters.access]
login_nodes = ["log001.pegasus.arc.gwu.edu", "log002.pegasus.arc.gwu.edu", "log003.pegasus.arc.gwu.edu", "log004.pegasus.arc.gwu.edu"]
require_vpn = true
prohibit_login_compute = true

[clusters.filesystems]
lustre_root = "/lustre"
groups_root = "/lustre/groups"
home_is_nfs = true
groups_is_nfs = true
scratch_recommended = true
scratch_purge_policy_url = "https://hpc.gwu.edu/new-colonial-one/purge-policy-for-pegasus-lustre-filesystem-lustre-groups/"

[clusters.slurm]
require_partition = true
require_time_limit = true
max_wall_time = "14-00:00:00"  # D-HH:MM:SS
default_partition = "defq"

Optional default time the server may inject if the caller omits one (still validated <= max)
default_time_limit = "01:00:00"

CPU partitions
[[clusters.slurm.partitions]]
name = "defq"
kind = "cpu"

[[clusters.slurm.partitions]]
name = "short"
kind = "cpu"

[[clusters.slurm.partitions]]
name = "tiny"
kind = "cpu"

[[clusters.slurm.partitions]]
name = "nano"
kind = "cpu"

[[clusters.slurm.partitions]]
name = "highThru"
kind = "cpu"
notes = "High clock, low core count; single-threaded jobs."

[[clusters.slurm.partitions]]
name = "highMem"
kind = "cpu"
notes = "Large memory nodes."

[[clusters.slurm.partitions]]
name = "debug-cpu"
kind = "cpu"
notes = "Short, small jobs for debugging."

GPU partitions
[[clusters.slurm.partitions]]
name = "small-gpu"
kind = "gpu"
gpus = ["V100 x2"]

[[clusters.slurm.partitions]]
name = "large-gpu"
kind = "gpu"
gpus = ["V100 x4 (NVLink)"]

[[clusters.slurm.partitions]]
name = "debug-gpu"
kind = "gpu"

[clusters.prompts]
slurm_safety_template = """
Always submit compute with Slurm using -p <partition> and -t <time>. cd to {{groups_root}}/<your_group>/<your_user> (Lustre) before heavy work. Do not run compute on login nodes.
"""

[clusters.policy]

Rendered to resource hpc://pegasus/policy
template = """
Pegasus ({{host}}) login nodes are for submission, data movement, compilation, and prep. Running jobs on login nodes is prohibited.
Compute must be submitted via Slurm with both -p and -t; maximum wall time is 14 days.
Use Lustre for active workloads: {{groups_root}}. Home and groups (NFS) are not for running jobs. Lustre is scratch and subject to purge; move long-term data elsewhere.
Inspect with sinfo; monitor with squeue; review with sacct. VPN/on-campus access and 2FA are required.
"""

Notes on the schema

Versioning: version = 1 allows future additions; the server logs and rejects unknown major versions.
Defaults vs cluster overrides: [defaults] set process behavior (pty, login shell, concurrency) and a default_cwd; clusters can override via their own values if we add fields later.
Heuristics:
denylist: quick safety checks that fail fast with a helpful error (can be disabled via CLI/env if needed).
compute_markers: warnings (or preflight validation) when suspicious commands are invoked on login sessions.
Slurm:
max_wall_time is authoritative for validate_slurm. The server will parse -t, compare, and suggest a fix if exceeded.
Partitions array is used to validate -p and offer alternatives when unknown.
Templates:
Keep simple {{var}} placeholders (we’ll implement a minimal renderer or use a tiny templating crate). Available vars include host, groups_root, lustre_root, and any top-level keys under clusters.filesystems and clusters.slurm.
Multiple clusters:
Add another [[clusters]] table; the MCP server can expose hpc://{id}/policy and choose a default via defaults.default_cluster or a per-call “cluster” parameter.
Example Pegasus file (ready-to-use; customize your group/user)

Values mapped from GWU docs:
Access and login nodes, VPN requirement, and prohibition on compute at the login tier. (hpc.gwu.edu)
Slurm partition families, required -p/-t, and 14-day max. (hpc.gwu.edu)
Lustre roots and purge policy. (hpc.gwu.edu)
Save as ~/.gwu-hpc-mcp/config.toml and replace <your_group>/<your_user> in defaults.default_cwd and prompts/policy templates.

Operational guidance for authors

If GWU staff rename or add partitions, only update the TOML and hot-reload the server; no rebuild required.
If your lab standardizes a scratch path (e.g., /lustre/groups/gedanlab/<user>), set defaults.default_cwd accordingly and tweak slurm_safety_template to remind the agent to cd there.
For organizations using multiple clusters (e.g., Pegasus and a teaching cluster), add multiple entries and set defaults.default_cluster as needed.
Validation the server should perform using this config

hpc.validate_slurm:
Parse a proposed sbatch/srun command.
Ensure -p and -t exist if clusters.slurm.require_* are true.
Verify -p is a known partition; if not, return an error plus known candidates.
Verify -t parses and is <= max_wall_time; if exceeded, return a suggested time <= cap.
Optionally inject defaults default_partition/default_time_limit when absent (configurable).
shell.exec preflight:
If a command matches heuristics.compute_markers and the server infers it’s on a login session, emit a warning or block with an actionable message referencing policy.
Resources/prompts:
Render the policy template to hpc://{id}/policy and the safety prompt to the slurm-safety prompt at startup and on reload.
Open questions to confirm with local policy owners

Exact GPU models on “small-gpu” nodes today (docs list V100s historically). If this varies, leave gpus unspecified or update when confirmed. (hpc.gwu.edu)
Any site-specific default_cwd conventions your lab wants pre-set.
Whether we should include a partitions.json resource alongside policy for machine parsing by clients.
