use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{info, warn};

use crate::config::{ConfigManager, SshBackend};
use crate::errors::{AppError, AppResult};
use crate::mcp::run_stdio;
use crate::ssh::{MasterState, SshContext};
use crate::util::expand_path;

mod broker;
#[cfg(test)]
mod broker_tests;
mod config;
mod errors;
mod ipc;
mod known_hosts;
mod mcp;
mod policy;
mod sessions;
mod ssh;
mod upload;
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
    /// Connect to the cluster, answering password/2FA prompts in this terminal
    Up(UpArgs),
    /// Close the shared cluster connection
    Down,
    /// Show the cluster connection status
    Status,
    /// Run a quick exec test (hostname && pwd)
    Test,
    /// Run diagnostics against the current configuration
    Doctor,
    /// Configuration helpers
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Internal: the background process behind `up` for the built-in SSH client
    #[command(hide = true)]
    Broker,
}

#[derive(Args, Debug, Clone)]
struct UpArgs {
    /// Built-in SSH client only: keep the connection in this terminal instead
    /// of a background process (close it with Ctrl-C)
    #[arg(long)]
    foreground: bool,
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
    /// Scratch working directory (e.g. /scratch/<group>/<user>)
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
        Command::Up(args) => cmd_up(&config, &remote_user, cli.cluster.as_deref(), args).await,
        Command::Broker => cmd_broker(&config, &remote_user, cli.cluster.as_deref()).await,
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
        .with_writer(std::io::stderr)
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
    let home = crate::util::home_dir().unwrap_or_else(|_| PathBuf::from("."));
    home.join(".gwu-hpc-mcp").join("config.toml")
}

fn resolve_remote_user(cli_user: Option<String>) -> AppResult<String> {
    if let Some(user) = cli_user {
        return Ok(user);
    }
    if let Ok(env_user) = std::env::var("GWU_HPC_MCP_USER") {
        return Ok(env_user);
    }
    // USERNAME is the Windows equivalent of USER.
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .map_err(|_| AppError::InvalidInput("Unable to determine remote user".to_string()))
}

async fn cmd_up(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
    args: UpArgs,
) -> AppResult<()> {
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster.clone(), remote_user.to_string());
    let endpoint = context.connection_endpoint()?;
    if context.backend() == SshBackend::Builtin {
        println!(
            "Connecting to {} with the built-in SSH client ({})",
            cluster.display_name(),
            endpoint
        );
        broker::up(
            &context,
            broker::UpOptions {
                foreground: args.foreground,
                config_path: absolute(config.path()),
            },
        )
        .await?;
        if !args.foreground {
            println!(
                "Cluster connection is active. It stays open after this terminal closes; run `down` to close it."
            );
        }
        return Ok(());
    }
    if args.foreground {
        return Err(AppError::InvalidInput(
            "--foreground applies to the built-in SSH client (ssh.backend = \"builtin\")".into(),
        ));
    }
    println!(
        "Starting ControlMaster for {} (socket: {})",
        cluster.display_name(),
        endpoint
    );
    ssh::spawn_master_interactive(&context).await?;
    println!("ControlMaster is now active.");
    Ok(())
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn cmd_broker(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
) -> AppResult<()> {
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster, remote_user.to_string());
    info!("broker starting (pid {})", std::process::id());
    let result = broker::run(&context, broker::AuthVia::Attach).await;
    match &result {
        Ok(()) => info!("broker exiting"),
        Err(e) => warn!("broker exiting: {e}"),
    }
    result
}

async fn cmd_down(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
) -> AppResult<()> {
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster.clone(), remote_user.to_string());
    println!(
        "Closing the cluster connection for {}",
        cluster.display_name()
    );
    ssh::shutdown_master(&context).await?;
    println!("Cluster connection closed.");
    Ok(())
}

async fn cmd_status(
    config: &ConfigManager,
    remote_user: &str,
    cluster_opt: Option<&str>,
) -> AppResult<()> {
    let cluster = config.get_cluster(cluster_opt)?;
    let context = SshContext::new(cluster.clone(), remote_user.to_string());
    let endpoint = context.connection_endpoint()?;
    let state = ssh::check_master(&context).await?;
    println!("Cluster: {}", cluster.display_name());
    println!("SSH backend: {}", backend_name(context.backend()));
    println!("Connection: {endpoint}");
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
    println!("SSH backend: {}", backend_name(context.backend()));
    println!("tmux binary (remote): {}", context.cluster.ssh.tmux_binary);
    if context.backend() == SshBackend::Builtin {
        let settings = broker::Settings::from_context(&context)?;
        println!("known_hosts: {}", settings.known_hosts.display());
        let keys: Vec<String> = settings
            .identity_files
            .iter()
            .filter(|p| p.exists())
            .map(|p| p.display().to_string())
            .collect();
        if keys.is_empty() {
            println!("Identity files: none found (password and 2FA prompts will be used)");
        } else {
            println!("Identity files: {}", keys.join(", "));
        }
    } else {
        println!("SSH binary: {}", context.cluster.ssh.ssh_binary);
        match which(&context.cluster.ssh.ssh_binary) {
            Ok(path) => println!("Found ssh at {}", path.display()),
            Err(_) => println!("WARNING: ssh binary not found in PATH"),
        }
        if cfg!(windows) {
            println!(
                "WARNING: Windows OpenSSH cannot share connections (no ControlMaster); set ssh.backend = \"builtin\" or \"auto\""
            );
        }
    }
    println!("tmux and GNU timeout must be available on the remote cluster (not locally).");
    println!("Connection: {}", context.connection_endpoint()?);
    match ssh::check_master(&context).await {
        Ok(MasterState::Running) => println!("Cluster connection: running"),
        Ok(MasterState::Missing) => println!("Cluster connection: missing (run `up`)"),
        Ok(MasterState::Stale) => println!("Cluster connection: stale (run `down` then `up`)"),
        Err(err) => println!("Cluster connection check failed: {}", err),
    }
    Ok(())
}

fn backend_name(backend: SshBackend) -> &'static str {
    match backend {
        SshBackend::Builtin => "built-in SSH client",
        _ => "OpenSSH ControlMaster",
    }
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
    let mut config: crate::config::Config =
        toml::from_str(include_str!("../example/pegasus.config.toml"))
            .expect("bundled config must parse");
    config.defaults.default_cluster = Some(cluster_id.into());
    config.defaults.default_cwd = Some(default_cwd.into());
    let cluster = &mut config.clusters[0];
    cluster.id = cluster_id.into();
    cluster.display_name = Some(display_name.into());
    cluster.host = host.into();
    cluster.filesystems.lustre_root = Some(scratch_root.into());
    cluster.filesystems.groups_root = Some(scratch_root.into());
    toml::to_string_pretty(&config).expect("config must serialize")
}

#[cfg(test)]
mod generation_tests {
    #[test]
    fn generated_config_escapes_values_and_preserves_regexes() {
        let path = "/scratch/group/user \"quoted\"";
        let text = super::build_default_config("test", "Test", "host", path, "/scratch");
        let config: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(config.defaults.default_cwd.as_deref(), Some(path));
        for command in ["srun hostname", "mpirun app"] {
            assert!(
                config
                    .heuristics
                    .compute_markers
                    .iter()
                    .any(|p| regex::Regex::new(p).unwrap().is_match(command))
            );
        }
        assert_eq!(
            config.clusters[0]
                .slurm
                .as_ref()
                .unwrap()
                .default_partition
                .as_deref(),
            Some("cpu")
        );
    }
}
