use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use schemars::JsonSchema;
use serde::Serialize;

use crate::config::{Cluster, ClusterFilesystems};

static TEMPLATE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{\{\s*([a-zA-Z0-9_]+)\s*\}\}").expect("valid template regex"));

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RenderedResources {
    pub policy: String,
    pub partitions: String,
    pub filesystem: String,
    pub slurm_prompt: Option<String>,
}

pub fn render_resources(cluster: &Cluster) -> RenderedResources {
    let vars = template_vars(cluster);
    RenderedResources {
        policy: render_policy(cluster, &vars),
        partitions: render_partitions(cluster),
        filesystem: render_filesystem(&cluster.filesystems),
        slurm_prompt: render_slurm_prompt(cluster, &vars),
    }
}

fn render_policy(cluster: &Cluster, vars: &HashMap<&'static str, String>) -> String {
    if let Some(template) = cluster.policy.template.as_deref() {
        return render_template(template, vars);
    }
    let mut lines = Vec::new();
    lines.push(format!(
        "{} login nodes are for submission, data movement, and prep work. \
        Run compute via Slurm from scratch directories.",
        cluster.display_name()
    ));
    if let Some(max) = cluster
        .slurm
        .as_ref()
        .and_then(|s| s.max_wall_time.as_deref())
    {
        lines.push(format!(
            "Jobs must include -p <partition> and -t <time>. Maximum wall time is {max}."
        ));
    }
    if let Some(groups_root) = cluster.filesystems.groups_root.as_deref() {
        lines.push(format!(
            "Use Shared scratch (e.g., {groups_root}/<group>/<user>) for active workloads."
        ));
    }
    if cluster.filesystems.home_is_nfs {
        lines.push("Home directories are NFS backed; avoid heavy compute there.".to_string());
    }
    if has_gpu_partition(cluster) {
        lines.push(
            "For quick GPU probes, interactive `srun --pty` can be flaky; prefer `sbatch --wrap \"hostname; nvidia-smi\"`."
                .to_string(),
        );
    }
    lines.join("\n")
}

fn render_partitions(cluster: &Cluster) -> String {
    match cluster.slurm.as_ref() {
        Some(slurm) if !slurm.partitions.is_empty() => {
            let mut lines = Vec::new();
            lines.push(format!("Known partitions for {}:", cluster.display_name()));
            for part in &slurm.partitions {
                let mut detail = format!(" - {}", part.name);
                if let Some(kind) = part.kind.as_deref() {
                    detail.push_str(&format!(" ({kind})"));
                }
                if let Some(notes) = part.notes.as_deref() {
                    detail.push_str(&format!(" — {}", notes));
                }
                lines.push(detail);

                if let Some(node_count) = part.node_count {
                    lines.push(format!("   Nodes in partition: {node_count}"));
                }

                if part.cpu_model.is_some()
                    || part.cpu_cores.is_some()
                    || part.cpu_clock_ghz.is_some()
                {
                    let mut pieces = Vec::new();
                    if let Some(model) = part.cpu_model.as_deref() {
                        pieces.push(model.to_string());
                    }
                    if let Some(cores) = part.cpu_cores {
                        pieces.push(format!("{cores} cores"));
                    }
                    if let Some(clock) = part.cpu_clock_ghz {
                        pieces.push(format!("{clock:.2} GHz"));
                    }
                    lines.push(format!("   CPU: {}", pieces.join(" · ")));
                }

                if let Some(ram) = part.ram_gb {
                    lines.push(format!("   RAM: {ram} GB"));
                }

                if part.gpu_model.is_some()
                    || part.gpu_count.is_some()
                    || part.gpu_memory_gb.is_some()
                    || part.gpu_total_memory_gb.is_some()
                    || !part.gpus.is_empty()
                {
                    let mut pieces = Vec::new();
                    match (part.gpu_count, part.gpu_model.as_deref()) {
                        (Some(count), Some(model)) => {
                            pieces.push(format!("{count} × {model}"));
                        }
                        (Some(count), None) => {
                            pieces.push(format!("{count} GPU(s)"));
                        }
                        (None, Some(model)) => pieces.push(model.to_string()),
                        (None, None) => {}
                    }
                    if let Some(mem) = part.gpu_memory_gb {
                        pieces.push(format!("{mem} GB each"));
                    }
                    if let Some(total) = part.gpu_total_memory_gb {
                        pieces.push(format!("{total} GB total"));
                    }
                    if pieces.is_empty() && !part.gpus.is_empty() {
                        pieces.push(part.gpus.join(", "));
                    }
                    if !pieces.is_empty() {
                        lines.push(format!("   GPU: {}", pieces.join(" · ")));
                    }
                }

                if let Some(interconnect) = part.interconnect.as_deref() {
                    lines.push(format!("   Interconnect: {interconnect}"));
                }
                if let Some(storage) = part.node_local_storage.as_deref() {
                    lines.push(format!("   Node-local storage: {storage}"));
                }
            }
            lines.join("\n")
        }
        _ => "No partitions listed in configuration.".to_string(),
    }
}

fn render_filesystem(fs: &ClusterFilesystems) -> String {
    let mut lines = Vec::new();
    if let Some(lustre) = fs.lustre_root.as_deref() {
        lines.push(format!(
            "Shared scratch root: {lustre}. Use for job working directories."
        ));
    }
    if let Some(groups_root) = fs.groups_root.as_deref() {
        lines.push(format!("Group scratch under {groups_root}/<group>/<user>."));
    }
    if fs.home_is_nfs {
        lines.push("Home directories are NFS; avoid running compute there.".to_string());
    }
    if fs.groups_is_nfs {
        lines.push("Group directories are NFS; prefer scratch for heavy workloads.".to_string());
    }
    if fs.scratch_recommended {
        lines.push("Prefer shared scratch for performance.".to_string());
    }
    if let Some(url) = fs.scratch_purge_policy_url.as_deref() {
        lines.push(format!("Scratch purge policy: {url}"));
    }
    if lines.is_empty() {
        lines.push("Filesystem guidance not available in config.".to_string());
    }
    lines.join("\n")
}

fn render_slurm_prompt(cluster: &Cluster, vars: &HashMap<&'static str, String>) -> Option<String> {
    cluster.prompts.slurm_safety_template.as_deref().map(|tpl| {
        let mut text = render_template(tpl, vars);
        if has_gpu_partition(cluster) {
            text.push_str(
                "\nNote: interactive `srun --pty` can be flaky; for quick GPU checks prefer `sbatch --wrap \"hostname; nvidia-smi\"`.",
            );
        }
        text
    })
}

fn render_template(template: &str, vars: &HashMap<&'static str, String>) -> String {
    TEMPLATE_RE
        .replace_all(template, |caps: &regex::Captures| {
            let key = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            vars.get(key).cloned().unwrap_or_default()
        })
        .into_owned()
}

fn template_vars(cluster: &Cluster) -> HashMap<&'static str, String> {
    let mut vars = HashMap::new();
    vars.insert("host", cluster.host.clone());
    vars.insert("display_name", cluster.display_name().to_string());
    if let Some(groups_root) = cluster.filesystems.groups_root.clone() {
        vars.insert("groups_root", groups_root);
    }
    if let Some(lustre_root) = cluster.filesystems.lustre_root.clone() {
        vars.insert("lustre_root", lustre_root);
    }
    if let Some(slurm) = &cluster.slurm {
        if let Some(max) = slurm.max_wall_time.clone() {
            vars.insert("max_wall_time", max);
        }
        if let Some(default_partition) = slurm.default_partition.clone() {
            vars.insert("default_partition", default_partition);
        }
        if let Some(default_time) = slurm.default_time_limit.clone() {
            vars.insert("default_time_limit", default_time);
        }
    }
    vars
}

fn has_gpu_partition(cluster: &Cluster) -> bool {
    cluster
        .slurm
        .as_ref()
        .map(|slurm| {
            slurm.partitions.iter().any(|p| {
                matches!(p.kind.as_deref(), Some("gpu"))
                    || !p.gpus.is_empty()
                    || p.gpu_model.is_some()
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClusterAccess, ClusterPolicy, ClusterPrompts, ClusterSsh};

    #[test]
    fn template_renders_variables() {
        let cluster = Cluster {
            id: "pegasus".into(),
            display_name: Some("Pegasus".into()),
            host: "pegasus.arc.gwu.edu".into(),
            port: 22,
            ssh: ClusterSsh::default(),
            access: ClusterAccess::default(),
            filesystems: ClusterFilesystems {
                lustre_root: Some("/lustre".into()),
                groups_root: Some("/lustre/groups".into()),
                ..Default::default()
            },
            slurm: None,
            prompts: ClusterPrompts::default(),
            policy: ClusterPolicy::default(),
        };
        let vars = template_vars(&cluster);
        let rendered = render_template("Cluster {{display_name}} host: {{host}}", &vars);
        assert_eq!(rendered, "Cluster Pegasus host: pegasus.arc.gwu.edu");
    }
}
