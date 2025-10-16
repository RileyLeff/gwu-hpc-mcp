use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{info, warn};

use crate::config::ConfigManager;
use crate::errors::{AppError, AppResult};
use crate::mcp::run_stdio;
use crate::ssh::{MasterState, SshContext};

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

#[derive(Subcommand)]
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
}

#[tokio::main]
async fn main() {
    if let Err(err) = async_main().await {
        eprintln!("Error: {}", err);
        std::process::exit(1);
    }
}

async fn async_main() -> AppResult<()> {
    init_tracing();
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config)?;
    let config = ConfigManager::from_path(config_path.clone())?;
    let remote_user = resolve_remote_user(cli.user)?;

    match cli.cmd.unwrap_or(Command::ServeStdio) {
        Command::ServeStdio => {
            let _watcher = start_config_watcher(config.clone()).ok();
            run_stdio(config, remote_user).await
        }
        Command::Up => cmd_up(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Down => cmd_down(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Status => cmd_status(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Test => cmd_test(&config, &remote_user, cli.cluster.as_deref()).await,
        Command::Doctor => cmd_doctor(&config, &remote_user, cli.cluster.as_deref()).await,
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
