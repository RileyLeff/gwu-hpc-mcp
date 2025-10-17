use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{info, warn};

use crate::config::ConfigManager;
use crate::errors::{AppError, AppResult};
use crate::mcp::run_stdio;
use crate::ssh::{MasterState, SshContext};
use crate::util::expand_path;

mod config;
mod errors;
mod mcp;
mod policy;
mod sessions;
mod ssh;
mod util;

#[derive(Parser)]
#[command(name = "gwu-hpc-mcp")]
#[command(about = "MCP server exposing GWU Pegasus via SSH/tmux", version)]
struct Cli {
    /// Path to the configuration TOML file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Remote cluster identifier
    #[arg(long, global = true)]
    cluster: Option<String>,

    /// Remote username (defaults to $GWU_HPC_MCP_USER or $USER)
    #[arg(long, global = true)]
    user: Option<String>,

    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand, Clone)]
enum Command {
    /// Run the MCP stdio server
    ServeStdio,
    /// Establish the SSH ControlMaster interactively
    Up,
    /// Terminate the SSH ControlMaster
    Down,
    /// Show ControlMaster status
    Status,
    /// Run a quick exec test (hostname && pwd)
    Test,
    /// Run diagnostics against the current configuration
    Doctor,
    /// Configuration helpers
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Subcommand, Clone)]
enum ConfigCommand {
    /// Initialize a configuration file with sensible defaults
    Init(ConfigInitArgs),
}

#[derive(Args, Debug, Clone)]
struct ConfigInitArgs {
    /// Output path for the generated config (default: ~/.gwu-hpc-mcp/config.toml)
    #[arg(long)]
    output: Option<PathBuf>,
    /// Scratch working directory (e.g. /scratch/groups/<group>/<user>)
    #[arg(long)]
    default_cwd: String,
    /// Scratch root path (default: /scratch)
    #[arg(long, default_value = "/scratch")]
    scratch_root: String,
    /// Cluster identifier used in MCP URIs (default: pegasus)
    #[arg(long, default_value = "pegasus")]
    cluster_id: String,
    /// Cluster display name (default: GWU Pegasus)
    #[arg(long, default_value = "GWU Pegasus")]
    display_name: String,
    /// SSH host (default: pegasus.arc.gwu.edu)
    #[arg(long, default_value = "pegasus.arc.gwu.edu")]
    host: String,
    /// Overwrite existing file if present
    #[arg(long)]
    force: bool,
}

pub async fn async_main() -> AppResult<()> {
    init_tracing();
    let cli = Cli::parse();

    let command = cli.cmd.unwrap_or(Command::ServeStdio);

    if let Command::Config(ConfigCommand::Init(args)) = &command {
        config_init(args.clone())?;
        return Ok(());
    }

    let config_path = resolve_config_path(cli.config)?;
    let config = ConfigManager::from_path(config_path.clone())?;
    let remote_user = resolve_remote_user(cli.user)?;

    match command {
        Command::ServeStdio => {
            let _watcher = start_config_watcher(config.clone()).ok();
            run_stdio(config, remote_user).await
        }
        Command::Up => cmd_up(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Down => cmd_down(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Status => cmd_status(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Test => cmd_test(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Doctor => cmd_doctor(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Config(ConfigCommand::Init(_)) => unreachable!(),
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gwu_hpc_mcp=info".into()),
        )
        .with_target(false)
        .try_init();
}

fn resolve_config_path(cli_path: Option<PathBuf>) -> AppResult<PathBuf> {
    if let Some(path) = cli_path {
        return Ok(path);
    }
    if let Ok(env_path) = std::env::var("GWU_HPC_MCP_CONFIG") {
        return Ok(PathBuf::from(env_path));
    }
    Ok(default_config_path())
}

fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".gwu-hpc-mcp").join("config.toml")
}

fn resolve_remote_user(cli_user: Option<String>) -> AppResult<String> {
    if let Some(user) = cli_user {
        return Ok(user);
    }
    if let Ok(env_user) = std::env::var("GWU_HPC_MCP_USER") {
        return Ok(env_user);
    }
    std::env::var("USER")
        .map_err(|_| AppError::InvalidInput("Unable to determine remote user".to_string()))
}

async fn cmd_up(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
) -> AppResult<()> {
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster.clone(), remote_user.to_string());
    let control_path = context.control_path()?;
    println!(
        "Starting ControlMaster for {} (socket: {})",
        cluster.display_name(),
        control_path.display()
    );
    ssh::spawn_master_interactive(&context).await?;
    println!("ControlMaster is now active.");
    Ok(())
}

async fn cmd_down(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
) -> AppResult<()> {
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster.clone(), remote_user.to_string());
    println!("Shutting down ControlMaster for {}", cluster.display_name());
    ssh::shutdown_master(&context).await?;
    println!("ControlMaster terminated.");
    Ok(())
}

async fn cmd_status(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
) -> AppResult<()> {
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster.clone(), remote_user.to_string());
    let control_path = context.control_path()?;
    let state = ssh::check_master(&context).await?;
    println!("Cluster: {}", cluster.display_name());
    println!("ControlPath: {}", control_path.display());
    match state {
        MasterState::Running => println!("Status: running"),
        MasterState::Missing => println!("Status: missing (run `gwu-hpc-mcp up`)"),
        MasterState::Stale => println!("Status: stale (run `gwu-hpc-mcp down` then `up`)"),
    }
    Ok(())
}

async fn cmd_test(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
) -> AppResult<()> {
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster.clone(), remote_user.to_string());
    ensure_master(&context, remote_user).await?;
    println!("Running hostname && pwd on {}", cluster.display_name());
    let req = ssh::ExecRequest {
        command: "hostname && pwd".to_string(),
        cwd: cluster
            .filesystems
            .lustre_root
            .clone()
            .or_else(|| cluster.filesystems.groups_root.clone()),
        env: HashMap::new(),
        pty: true,
        login_shell: true,
        timeout: Some(Duration::from_secs(30)),
    };
    let mut output = String::new();
    let mut collector = |_: &str, chunk: &[u8]| {
        output.push_str(&String::from_utf8_lossy(chunk));
    };
    let result = match ssh::exec(&context, req, &mut collector).await {
        Ok(res) => res,
        Err(failure) => {
            println!("Command failed: {}", failure.error);
            if !failure.tail.is_empty() {
                println!("Tail:\n{}", failure.tail);
            }
            return Err(failure.error);
        }
    };
    println!("{}", output.trim());
    println!("Exit code: {}", result.exit_code);
    Ok(())
}

async fn cmd_doctor(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
) -> AppResult<()> {
    use which::which;
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster.clone(), remote_user.to_string());
    println!("Configuration path: {}", config.path().display());
    println!("Target cluster: {}", cluster.display_name());
    println!("SSH binary: {}", context.cluster.ssh.ssh_binary);
    println!("tmux binary: {}", context.cluster.ssh.tmux_binary);
    match which(&context.cluster.ssh.ssh_binary) {
        Ok(path) => println!("Found ssh at {}", path.display()),
        Err(_) => println!("WARNING: ssh binary not found in PATH"),
    }
    match which(&context.cluster.ssh.tmux_binary) {
        Ok(path) => println!("Found tmux at {}", path.display()),
        Err(_) => println!("WARNING: tmux binary not found in PATH"),
    }
    let control_path = context.control_path()?;
    println!("ControlPath: {}", control_path.display());
    match ssh::check_master(&context).await {
        Ok(MasterState::Running) => println!("ControlMaster: running"),
        Ok(MasterState::Missing) => println!("ControlMaster: missing"),
        Ok(MasterState::Stale) => println!("ControlMaster: stale"),
        Err(err) => println!("ControlMaster check failed: {}", err),
    }
    println!("Attempting ssh -O check...");
    match ssh::check_master(&context).await {
        Ok(state) => println!("ControlMaster state: {:?}", state),
        Err(err) => println!("ssh check failed: {}", err),
    }
    Ok(())
}

fn start_config_watcher(config: ConfigManager) -> notify::Result<RecommendedWatcher> {
    let path = config.path().to_path_buf();
    let log_path = path.clone();
    let mut watcher = notify::recommended_watcher({
        move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                if matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                ) {
                    match config.reload() {
                        Ok(_) => info!("Reloaded config {}", log_path.display()),
                        Err(err) => warn!("Failed to reload config: {}", err),
                    }
                }
            }
            Err(err) => warn!("Config watcher error: {}", err),
        }
    })?;
    watcher.watch(&path, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

async fn ensure_master(context: &SshContext, remote_user: &str) -> AppResult<()> {
    match ssh::check_master(context).await? {
        MasterState::Running => Ok(()),
        MasterState::Missing | MasterState::Stale => Err(AppError::Needs2fa(format!(
            "Run: gwu-hpc-mcp up --cluster {} --user {}",
            context.cluster.id, remote_user
        ))),
    }
}

fn config_init(args: ConfigInitArgs) -> AppResult<()> {
    let output = args.output.unwrap_or_else(default_config_path);
    let expanded = expand_path(
        output
            .to_str()
            .ok_or_else(|| AppError::Config("Invalid output path".to_string()))?,
    )?;

    if expanded.exists() && !args.force {
        return Err(AppError::Config(format!(
            "Config file {} already exists (use --force to overwrite)",
            expanded.display()
        )));
    }

    if let Some(parent) = expanded.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AppError::Io(format!(
                "Failed to create config directory {}: {e}",
                parent.display()
            ))
        })?;
    }

    let template = build_default_config(
        &args.cluster_id,
        &args.display_name,
        &args.host,
        &args.default_cwd,
        &args.scratch_root,
    );
    std::fs::write(&expanded, template).map_err(|e| {
        AppError::Io(format!(
            "Failed to write config {}: {e}",
            expanded.display()
        ))
    })?;
    println!("Wrote config to {}", expanded.display());
    Ok(())
}

fn build_default_config(
    cluster_id: &str,
    display_name: &str,
    host: &str,
    default_cwd: &str,
    scratch_root: &str,
) -> String {
    format!(
        r#"version = 1

[defaults]
pty = true
login_shell = true
max_concurrent_execs = 4
warn_on_login_compute = true
default_cluster = "{cluster_id}"
default_cwd = "{default_cwd}"

[heuristics]
denylist = ["rm -rf /", "mkfs", "shutdown", "reboot"]
compute_markers = [
  "CUDA_VISIBLE_DEVICES",
  "(?i)python.(torch|tensorflow|jax)",
  "\\bmpirun\\b",
  "\\bsrun\\b"
]

[[clusters]]
id = "{cluster_id}"
display_name = "{display_name}"
host = "{host}"
port = 22

[clusters.ssh]
control_path = "~/.gwu-hpc-mcp/cm-%r@%h_%p"
server_alive_interval = 60
server_alive_count_max = 3
ssh_binary = "ssh"
tmux_binary = "tmux"

[clusters.access]
login_nodes = [
  "log001.pegasus.arc.gwu.edu",
  "log002.pegasus.arc.gwu.edu",
  "log003.pegasus.arc.gwu.edu",
  "log004.pegasus.arc.gwu.edu"
]
require_vpn = true
prohibit_login_compute = true

[clusters.filesystems]
lustre_root = "{scratch_root}"
groups_root = "{scratch_root}"
home_is_nfs = true
groups_is_nfs = true
scratch_recommended = true
scratch_purge_policy_url = "https://hpc.gwu.edu/scratch-storage"

[clusters.slurm]
require_partition = true
require_time_limit = true
max_wall_time = "14-00:00:00"
default_partition = "defq"
default_time_limit = "01:00:00"

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
notes = "High clock, low core-count nodes; good for serial/high-throughput tasks."

[[clusters.slurm.partitions]]
name = "highMem"
kind = "cpu"
notes = "Large-memory nodes."

[[clusters.slurm.partitions]]
name = "highCore"
kind = "cpu"
notes = "High core-count CPU nodes (7-day limit)."

[[clusters.slurm.partitions]]
name = "384gb"
kind = "cpu"
notes = "384 GB memory CPU nodes."

[[clusters.slurm.partitions]]
name = "short-384gb"
kind = "cpu"
notes = "384 GB memory nodes with 1-day limit."

[[clusters.slurm.partitions]]
name = "debug"
kind = "cpu"
notes = "Generic debug queue (4 hours)."

[[clusters.slurm.partitions]]
name = "debug-cpu"
kind = "cpu"
notes = "CPU-only debug queue."

[[clusters.slurm.partitions]]
name = "small-gpu"
kind = "gpu"
gpus = ["V100 x2"]

[[clusters.slurm.partitions]]
name = "large-gpu"
kind = "gpu"
gpus = ["V100 x4 (NVLink)"]

[[clusters.slurm.partitions]]
name = "med-gpu"
kind = "gpu"
notes = "Medium GPU nodes; 7-day limit."

[[clusters.slurm.partitions]]
name = "debug-gpu"
kind = "gpu"

[[clusters.slurm.partitions]]
name = "ultra-gpu"
kind = "gpu"
notes = "Higher-end GPU nodes (7-day limit)."

[[clusters.slurm.partitions]]
name = "superChip"
kind = "gpu"
notes = "NVIDIA GH200 SuperChip nodes."

[[clusters.slurm.partitions]]
name = "viz"
kind = "gpu"
notes = "Visualization GPU node."

[clusters.prompts]
slurm_safety_template = """
Always submit compute with Slurm using -p <partition> and -t <time>. cd to {{{{groups_root}}}}/<your_group>/<your_user> (scratch) before heavy work. Do not run compute on login nodes.

Keep login-shell execution enabled (the default) so environment modules work. When exploring modules, prefer patterns like:
  module avail | grep <name>
  module spider <name>
  module keyword <term>
and pipe long listings through head/less to stay readable.
"""

[clusters.policy]
template = """
Pegasus ({{{{host}}}}) login nodes are for submission, data movement, compilation, and prep. Running jobs on login nodes is prohibited.
Compute must be submitted via Slurm with both -p and -t; maximum wall time is 14 days.
Use scratch for active workloads: {{{{groups_root}}}}. Home and groups (NFS) are not for running jobs. Scratch is purgeable; move long-term data elsewhere.
Inspect with sinfo; monitor with squeue; review with sacct. VPN/on-campus access and 2FA are required.

Environment modules load correctly when commands run inside a login shell; leave login_shell=true unless you have a specific reason not to. To discover modules without drowning in output, use:
  module spider <name>
  module keyword <term>
  module avail | grep <term>
  (pipe long listings to head/less)
"""
"#
    )
}
