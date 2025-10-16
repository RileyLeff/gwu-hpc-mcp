use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::errors::{AppError, AppResult};
use crate::util::expand_path;

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub heuristics: Heuristics,
    pub clusters: Vec<Cluster>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct Defaults {
    #[serde(default = "Defaults::default_pty")]
    pub pty: bool,
    #[serde(default = "Defaults::default_login_shell")]
    pub login_shell: bool,
    #[serde(default = "Defaults::default_max_concurrent_execs")]
    pub max_concurrent_execs: usize,
    #[serde(default = "Defaults::default_warn_on_login_compute")]
    pub warn_on_login_compute: bool,
    #[serde(default)]
    pub default_cluster: Option<String>,
    #[serde(default)]
    pub default_cwd: Option<String>,
}

impl Defaults {
    const fn default_pty() -> bool {
        true
    }

    const fn default_login_shell() -> bool {
        true
    }

    const fn default_max_concurrent_execs() -> usize {
        4
    }

    const fn default_warn_on_login_compute() -> bool {
        true
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            pty: Self::default_pty(),
            login_shell: Self::default_login_shell(),
            max_concurrent_execs: Self::default_max_concurrent_execs(),
            warn_on_login_compute: Self::default_warn_on_login_compute(),
            default_cluster: None,
            default_cwd: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct Heuristics {
    #[serde(default)]
    pub denylist: Vec<String>,
    #[serde(default)]
    pub compute_markers: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct Cluster {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub host: String,
    #[serde(default = "Cluster::default_port")]
    pub port: u16,
    #[serde(default)]
    pub ssh: ClusterSsh,
    #[serde(default)]
    pub access: ClusterAccess,
    #[serde(default)]
    pub filesystems: ClusterFilesystems,
    #[serde(default)]
    pub slurm: Option<ClusterSlurm>,
    #[serde(default)]
    pub prompts: ClusterPrompts,
    #[serde(default)]
    pub policy: ClusterPolicy,
}

impl Cluster {
    const fn default_port() -> u16 {
        22
    }

    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct ClusterSsh {
    pub control_path: String,
    #[serde(default = "ClusterSsh::default_server_alive_interval")]
    pub server_alive_interval: u64,
    #[serde(default = "ClusterSsh::default_server_alive_count_max")]
    pub server_alive_count_max: u64,
    #[serde(default = "ClusterSsh::default_ssh_binary")]
    pub ssh_binary: String,
    #[serde(default = "ClusterSsh::default_tmux_binary")]
    pub tmux_binary: String,
}

impl ClusterSsh {
    const fn default_server_alive_interval() -> u64 {
        60
    }

    const fn default_server_alive_count_max() -> u64 {
        3
    }

    fn default_ssh_binary() -> String {
        "ssh".to_string()
    }

    fn default_tmux_binary() -> String {
        "tmux".to_string()
    }
}

impl Default for ClusterSsh {
    fn default() -> Self {
        Self {
            control_path: "~/.gwu-hpc-mcp/cm-%r@%h_%p".to_string(),
            server_alive_interval: Self::default_server_alive_interval(),
            server_alive_count_max: Self::default_server_alive_count_max(),
            ssh_binary: Self::default_ssh_binary(),
            tmux_binary: Self::default_tmux_binary(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct ClusterAccess {
    #[serde(default)]
    pub login_nodes: Vec<String>,
    #[serde(default)]
    pub require_vpn: bool,
    #[serde(default)]
    pub prohibit_login_compute: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct ClusterFilesystems {
    #[serde(default)]
    pub lustre_root: Option<String>,
    #[serde(default)]
    pub groups_root: Option<String>,
    #[serde(default)]
    pub home_is_nfs: bool,
    #[serde(default)]
    pub groups_is_nfs: bool,
    #[serde(default)]
    pub scratch_recommended: bool,
    #[serde(default)]
    pub scratch_purge_policy_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct ClusterSlurm {
    #[serde(default)]
    pub require_partition: bool,
    #[serde(default)]
    pub require_time_limit: bool,
    #[serde(default)]
    pub max_wall_time: Option<String>,
    #[serde(default)]
    pub default_partition: Option<String>,
    #[serde(default)]
    pub default_time_limit: Option<String>,
    #[serde(default)]
    pub partitions: Vec<ClusterSlurmPartition>,
}

impl Default for ClusterSlurm {
    fn default() -> Self {
        Self {
            require_partition: true,
            require_time_limit: true,
            max_wall_time: None,
            default_partition: None,
            default_time_limit: None,
            partitions: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct ClusterSlurmPartition {
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub gpus: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct ClusterPrompts {
    #[serde(default)]
    pub slurm_safety_template: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, JsonSchema)]
pub struct ClusterPolicy {
    #[serde(default)]
    pub template: Option<String>,
}

#[derive(Clone)]
pub struct ConfigManager {
    inner: Arc<RwLock<ConfigState>>,
    notifier: watch::Sender<u64>,
    path: PathBuf,
}

struct ConfigState {
    raw: Config,
    compiled: CompiledConfig,
    version: u64,
}

#[derive(Clone)]
pub struct CompiledConfig {
    pub heuristics: CompiledHeuristics,
    pub default_cluster: Option<String>,
    clusters: HashMap<String, Arc<Cluster>>,
}

impl CompiledConfig {
    pub fn clusters(&self) -> impl Iterator<Item = Arc<Cluster>> + '_ {
        self.clusters.values().cloned()
    }
}

#[derive(Clone)]
pub struct CompiledHeuristics {
    pub denylist: Vec<Regex>,
    pub compute_markers: Vec<Regex>,
}

#[derive(Clone)]
pub struct ConfigSnapshot {
    pub config: Config,
    pub compiled: CompiledConfig,
    pub version: u64,
}

impl ConfigManager {
    pub fn from_path(path: PathBuf) -> AppResult<Self> {
        let resolved = expand_config_path(path)?;
        let state = load_config(&resolved)?;
        let version = 1;
        let (notifier, _) = watch::channel(version);
        Ok(Self {
            inner: Arc::new(RwLock::new(ConfigState {
                raw: state.config.clone(),
                compiled: state.compiled.clone(),
                version,
            })),
            notifier,
            path: resolved,
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.notifier.subscribe()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self) -> ConfigSnapshot {
        let guard = self.inner.read();
        ConfigSnapshot {
            config: guard.raw.clone(),
            compiled: guard.compiled.clone(),
            version: guard.version,
        }
    }

    pub fn reload(&self) -> AppResult<()> {
        let new_state = load_config(&self.path)?;
        let mut guard = self.inner.write();
        guard.raw = new_state.config.clone();
        guard.compiled = new_state.compiled.clone();
        guard.version += 1;
        let _ = self.notifier.send(guard.version);
        Ok(())
    }

    pub fn get_cluster(&self, cluster_opt: Option<&str>) -> AppResult<Arc<Cluster>> {
        let guard = self.inner.read();
        let compiled = &guard.compiled;
        let target_id = if let Some(id) = cluster_opt {
            id.to_string()
        } else if let Some(default_id) = compiled.default_cluster.clone() {
            default_id
        } else {
            return Err(AppError::Config(
                "No cluster specified and no default_cluster configured".to_string(),
            ));
        };
        let Some(cluster) = compiled.clusters.get(&target_id) else {
            let known = compiled
                .clusters
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AppError::Config(format!(
                "Unknown cluster '{target_id}'. Known: {known}"
            )));
        };
        Ok(cluster.clone())
    }

    pub fn compiled(&self) -> CompiledConfig {
        self.inner.read().compiled.clone()
    }
}

fn load_config(path: &Path) -> AppResult<ConfigSnapshot> {
    let text = fs::read_to_string(path)
        .map_err(|e| AppError::Config(format!("Failed to read config {}: {e}", path.display())))?;
    let config: Config = toml::from_str(&text)
        .map_err(|e| AppError::Config(format!("Failed to parse config {}: {e}", path.display())))?;
    if config.version != 1 {
        return Err(AppError::Config(format!(
            "Unsupported config version {}",
            config.version
        )));
    }
    let compiled = compile_config(&config)?;
    Ok(ConfigSnapshot {
        config,
        compiled,
        version: 1,
    })
}

fn expand_config_path(path: PathBuf) -> AppResult<PathBuf> {
    let as_str = path
        .to_str()
        .ok_or_else(|| AppError::Config("Invalid config path".to_string()))?;
    expand_path(as_str)
}

fn compile_config(config: &Config) -> AppResult<CompiledConfig> {
    let mut clusters = HashMap::new();
    for cluster in &config.clusters {
        if clusters.contains_key(&cluster.id) {
            return Err(AppError::Config(format!(
                "Duplicate cluster id '{}'",
                cluster.id
            )));
        }
        clusters.insert(cluster.id.clone(), Arc::new(cluster.clone()));
    }
    let denylist = config
        .heuristics
        .denylist
        .iter()
        .map(|pattern| Regex::new(pattern).map_err(|e| AppError::Config(e.to_string())))
        .collect::<AppResult<Vec<_>>>()?;
    let compute_markers = config
        .heuristics
        .compute_markers
        .iter()
        .map(|pattern| Regex::new(pattern).map_err(|e| AppError::Config(e.to_string())))
        .collect::<AppResult<Vec<_>>>()?;
    if let Some(default_cluster) = &config.defaults.default_cluster {
        if !clusters.contains_key(default_cluster) {
            return Err(AppError::Config(format!(
                "default_cluster '{}' not found in clusters",
                default_cluster
            )));
        }
    }
    Ok(CompiledConfig {
        heuristics: CompiledHeuristics {
            denylist,
            compute_markers,
        },
        default_cluster: config.defaults.default_cluster.clone(),
        clusters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip() {
        let raw = r#"
version = 1

[defaults]
default_cluster = "pegasus"

[heuristics]
denylist = []
compute_markers = []

[[clusters]]
id = "pegasus"
display_name = "Pegasus"
host = "pegasus.arc.gwu.edu"

[clusters.ssh]
control_path = "~/.gwu-hpc-mcp/cm-%r@%h_%p"

[clusters.access]
[clusters.filesystems]
[clusters.prompts]
[clusters.policy]
"#;

        let config: Config = toml::from_str(raw).expect("valid config");
        let compiled = compile_config(&config).expect("compiled config");
        assert_eq!(compiled.default_cluster.as_deref(), Some("pegasus"));
        let cluster = compiled.clusters().next().expect("cluster present");
        assert_eq!(cluster.host, "pegasus.arc.gwu.edu");
    }
}
