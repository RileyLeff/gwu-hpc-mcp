use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    Json, RoleServer, ServiceExt,
    handler::server::router::tool::ToolRouter,
    handler::server::tool::IntoCallToolResult,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResponse, CallToolResult, GetPromptResponse, GetPromptResult, Implementation,
        ListPromptsResult, ListResourcesResult, ProgressNotificationParam, Prompt, PromptArgument,
        PromptMessage, ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, Role,
        ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::{Cluster, ConfigManager, ConfigSnapshot};
use crate::errors::{AppError, AppResult};
use crate::policy::{RenderedResources, render_resources};
use crate::sessions::{SessionExecFailure, SessionManager};
use crate::ssh::{self, ExecFailure, ExecRequest, MasterState, SshContext};

#[derive(Clone)]
pub struct McpServer {
    config: ConfigManager,
    remote_user: String,
    tool_router: ToolRouter<Self>,
    concurrency: Arc<Semaphore>,
}

impl McpServer {
    pub fn new(config: ConfigManager, remote_user: String) -> Self {
        let snapshot = config.snapshot();
        let limit = snapshot.config.defaults.max_concurrent_execs.max(1);
        drop(snapshot);
        Self {
            config,
            remote_user,
            tool_router: Self::tool_router(),
            concurrency: Arc::new(Semaphore::new(limit)),
        }
    }

    fn snapshot(&self) -> ConfigSnapshot {
        self.config.snapshot()
    }

    fn acquire_cluster(&self, id: Option<&str>) -> AppResult<Arc<Cluster>> {
        self.config.get_cluster(id)
    }

    fn build_context(&self, cluster: Arc<Cluster>) -> SshContext {
        SshContext::new(cluster, self.remote_user.clone())
    }

    async fn acquire_permit(&self) -> AppResult<OwnedSemaphorePermit> {
        self.concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| AppError::Io(format!("Failed to acquire execution permit: {e}")))
    }
}

fn normalize_cluster_id<'a>(id: Option<&'a str>) -> Option<&'a str> {
    match id {
        Some(s) if s.trim().is_empty() => None,
        other => other,
    }
}

#[tool_router]
impl McpServer {
    #[tool(
        name = "upload_text_bundle",
        title = "Upload text files to the cluster",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        ),
        description = "Create a NEW source-text bundle under configured upload_root (or default_cwd). Contents travel as data on stdin, never shell code. Requires exact SHA-256 per file; verifies readback. Refuses existing destinations, traversal and symlink parents. Parent directory must exist. Up to 128 flat files / 8 MiB JSON, 30s timeout, mode 0600; never executes files. On failure inspect partial destination before retrying. Uses existing authenticated cluster connection. Does not relax shell_exec denylist."
    )]
    pub async fn upload_text_bundle(
        &self,
        Parameters(params): Parameters<crate::upload::UploadTextBundleParams>,
    ) -> Result<Json<ExecResponse>, ToolFailure<ExecResponse>> {
        let snapshot = self.snapshot();
        let root = snapshot
            .config
            .defaults
            .upload_root
            .as_deref()
            .or(snapshot.config.defaults.default_cwd.as_deref())
            .ok_or_else(|| {
                AppError::InvalidInput(
                    "upload requires configured upload_root or project default_cwd".into(),
                )
            })?;
        let (request, input) = crate::upload::prepare(root, &params)?;
        let cluster = self.acquire_cluster(normalize_cluster_id(params.cluster.as_deref()))?;
        let context = self.build_context(cluster);
        let _permit = self.acquire_permit().await?;
        ensure_master(&context, &self.remote_user).await?;
        let limit = snapshot.config.defaults.max_output_bytes;
        match ssh::exec_with_input(&context, request, input).await {
            Ok(result) => Ok(Json(
                ExecResponse::completed(
                    result,
                    vec!["Create-only upload; file contents were not executed.".into()],
                )
                .capped(limit),
            )),
            Err(failure) => Err(ToolFailure(ExecResponse::from(failure).capped(limit))),
        }
    }

    #[tool(
        name = "shell_exec",
        title = "Run a shell command on the cluster",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        ),
        description = "Run one shell command on a cluster login node over the user's authenticated SSH connection and return its exit code and output. \
            Each call starts a fresh shell: cd, exports and variables do not persist, so pass cwd and env instead. \
            pty=false keeps stdout and stderr as separate streams; pty=true merges them into one \"pty\" stream. \
            ok is true only when exit_code is 0. \
            Login nodes are shared: submit compute-heavy or long-running work through Slurm (sbatch) instead of running it here. \
            Times out after timeout_s seconds (default 120, [defaults].exec_timeout_s); cancelling the request also stops it. \
            Either way only the local SSH client is stopped: Slurm jobs already submitted keep running (use scancel). \
            A heuristic denylist rejects some dangerous commands. Read hpc_describe before remote work."
    )]
    pub async fn shell_exec(
        &self,
        Parameters(params): Parameters<ShellExecParams>,
        request: RequestContext<RoleServer>,
    ) -> Result<Json<ExecResponse>, ToolFailure<ExecResponse>> {
        crate::util::validate_env(&params.env)?;
        let snapshot = self.snapshot();
        let timeout_s = params
            .timeout_s
            .unwrap_or(snapshot.config.defaults.exec_timeout_s);
        if timeout_s == 0 {
            return Err(
                AppError::InvalidInput("timeout_s must be greater than zero".into()).into(),
            );
        }
        let cluster = self.acquire_cluster(normalize_cluster_id(params.cluster.as_deref()))?;
        let context = self.build_context(cluster.clone());
        let permit = self.acquire_permit().await?;
        ensure_master(&context, &self.remote_user).await?;

        if snapshot
            .compiled
            .heuristics
            .denylist
            .iter()
            .any(|re| re.is_match(&params.command))
        {
            return Err(AppError::DeniedByPolicy(params.command).into());
        }

        let mut status = Vec::new();
        if snapshot.config.defaults.warn_on_login_compute
            && snapshot
                .compiled
                .heuristics
                .compute_markers
                .iter()
                .any(|re| re.is_match(&params.command))
        {
            status.push("Command resembles compute workload; ensure Slurm submission (-p/-t) before running heavy tasks on login nodes.".to_string());
        }

        let cwd = params
            .cwd
            .or_else(|| snapshot.config.defaults.default_cwd.clone());
        let exec_request = ExecRequest {
            command: params.command.clone(),
            cwd,
            env: params.env,
            pty: params.pty.unwrap_or(snapshot.config.defaults.pty),
            login_shell: params
                .login_shell
                .unwrap_or(snapshot.config.defaults.login_shell),
            timeout: Some(Duration::from_secs(timeout_s)),
        };
        let mut progress = OutputProgress::start(&request);
        // Dropping the exec future on cancellation kills the local ssh child
        // (kill_on_drop), which closes the remote channel.
        let exec_result = tokio::select! {
            result = ssh::exec(&context, exec_request, |_, chunk: &[u8]| {
                if let Some(progress) = progress.as_mut() {
                    progress.record(chunk.len());
                }
            }) => Some(result),
            _ = request.ct.cancelled() => None,
        };
        drop(permit);
        if let Some(progress) = progress {
            progress.finish().await;
        }
        let Some(exec_result) = exec_result else {
            return Err(AppError::Cancelled.into());
        };
        let limit = snapshot.config.defaults.max_output_bytes;
        match exec_result {
            Ok(result) => Ok(Json(ExecResponse::completed(result, status).capped(limit))),
            Err(failure) => Err(ToolFailure(ExecResponse::from(failure).capped(limit))),
        }
    }

    #[tool(
        name = "session_open",
        title = "Open a tmux session",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        ),
        description = "Open a named, detached tmux session on the cluster for a series of session_exec calls. \
            Fails if a session with that name already exists. Use a distinct name per task and never drive or close another client's sessions."
    )]
    pub async fn session_open(
        &self,
        Parameters(params): Parameters<SessionOpenParams>,
    ) -> Result<Json<SessionResponse>, ToolFailure<SessionResponse>> {
        let cluster = self.acquire_cluster(normalize_cluster_id(params.cluster.as_deref()))?;
        let context = self.build_context(cluster.clone());
        ensure_master(&context, &self.remote_user).await?;
        let manager = SessionManager::new(context);
        manager.open(&params.name, params.cwd.as_deref()).await?;
        Ok(Json(SessionResponse::done(format!(
            "Session '{}' opened on {}",
            params.name,
            cluster.display_name()
        ))))
    }

    #[tool(
        name = "session_exec",
        title = "Run a command in a tmux session",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        ),
        description = "Run a command inside an existing tmux session (see session_open) and wait for it to finish. \
            Each command runs in a fresh login Bash child: cd, exports and shell variables do NOT persist between calls, so pass cwd and env every time. \
            stdout and stderr are merged into one \"tmux\" stream, buffered in private remote temp files and returned on completion. \
            Defaults to a 300-second timeout; exit codes 124/137 mean the command timed out. Commands to the same session run one at a time. \
            Submit compute-heavy or long-running work through Slurm (sbatch) instead of running it on the login node."
    )]
    pub async fn session_exec(
        &self,
        Parameters(params): Parameters<SessionExecParams>,
    ) -> Result<Json<ExecResponse>, ToolFailure<ExecResponse>> {
        let limit = self.snapshot().config.defaults.max_output_bytes;
        let cluster = self.acquire_cluster(normalize_cluster_id(params.cluster.as_deref()))?;
        let context = self.build_context(cluster.clone());
        ensure_master(&context, &self.remote_user).await?;
        let manager = SessionManager::new(context);
        let result = manager
            .exec(
                &params.name,
                &params.command,
                params.cwd.as_deref(),
                &params.env,
                params.timeout_s.map(Duration::from_secs),
            )
            .await
            .map_err(|failure| ToolFailure(ExecResponse::from(failure).capped(limit)))?;
        Ok(Json(
            ExecResponse {
                ok: result.exit_code == 0,
                exit_code: Some(result.exit_code),
                duration_ms: result.duration.as_millis() as u64,
                streams: HashMap::from([("tmux".to_string(), result.output)]),
                ..Default::default()
            }
            .capped(limit),
        ))
    }

    #[tool(
        name = "session_close",
        title = "Close a tmux session",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        ),
        description = "Kill a tmux session and any process still running in it. Only close sessions this client opened."
    )]
    pub async fn session_close(
        &self,
        Parameters(params): Parameters<SessionCloseParams>,
    ) -> Result<Json<SessionResponse>, ToolFailure<SessionResponse>> {
        let cluster = self.acquire_cluster(normalize_cluster_id(params.cluster.as_deref()))?;
        let context = self.build_context(cluster.clone());
        ensure_master(&context, &self.remote_user).await?;
        let manager = SessionManager::new(context);
        manager.close(&params.name).await?;
        Ok(Json(SessionResponse::done(format!(
            "Session '{}' closed on {}",
            params.name,
            cluster.display_name()
        ))))
    }

    #[tool(
        name = "hpc_describe",
        title = "Describe cluster policies",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Return the configured policy, filesystem guidance, Slurm partitions and module-loading notes for a cluster. \
            Read it before submitting jobs, moving data or setting up environments. \
            It is a configured snapshot: verify current resources with sinfo or scontrol."
    )]
    pub async fn hpc_describe(
        &self,
        Parameters(params): Parameters<ClusterScopedParams>,
    ) -> Result<String, ToolFailure<ToolErrorInfo>> {
        let snapshot = self.snapshot();
        let cluster = self.acquire_cluster(normalize_cluster_id(params.cluster.as_deref()))?;
        let resources = render_resources(&cluster);

        let output = HpcDescribeOutput {
            cluster: (*cluster).clone(),
            config_version: snapshot.version,
            resources,
        };

        let details = toml::to_string_pretty(&output)
            .map_err(|e| AppError::Io(format!("Failed to serialize describe output: {e}")))?;
        Ok(details)
    }

    #[tool(
        name = "health_check",
        title = "Check the cluster connection",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Report whether the user's local SSH ControlMaster (the authenticated cluster connection) is Running, Missing or Stale. \
            It does not contact the cluster; run a harmless shell_exec such as `hostname` to confirm end-to-end access. \
            If it is not Running, ask the user to reconnect with `ghm up` in their own terminal. Never guess, request or submit 2FA codes."
    )]
    pub async fn health_check(
        &self,
        Parameters(params): Parameters<ClusterScopedParams>,
    ) -> Result<Json<HealthCheckResponse>, ToolFailure<HealthCheckResponse>> {
        let snapshot = self.snapshot();
        let cluster = self.acquire_cluster(normalize_cluster_id(params.cluster.as_deref()))?;
        let context = self.build_context(cluster.clone());
        let control_path = context.control_path()?;
        let state = ssh::check_master(&context).await?;
        let (state_str, message) = match state {
            MasterState::Running => ("Running".to_string(), None),
            MasterState::Missing => (
                "Missing".to_string(),
                Some(format!(
                    "Cluster connection is missing. Tell the user to check GW VPN if off campus, then run ghm up --cluster {} --user {} in their terminal and complete 2FA themselves. Never guess or submit codes.",
                    cluster.id, self.remote_user
                )),
            ),
            MasterState::Stale => (
                "Stale".to_string(),
                Some("Cluster connection appears stale. Tell the user to check GW VPN and reconnect with ghm up in their terminal. Never guess or submit 2FA codes. Coordinate before resetting a connection shared with other clients.".to_string()),
            ),
        };
        Ok(Json(HealthCheckResponse {
            master_state: state_str,
            control_path: control_path.to_str().unwrap_or_default().to_string(),
            message,
            config_version: snapshot.version,
            error: None,
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_server_info(
            Implementation::new("gwu-hpc-mcp", env!("CARGO_PKG_VERSION"))
                .with_title("GWU HPC cluster")
                .with_description(
                    "Run commands, tmux sessions and uploads on GWU's Pegasus HPC cluster over the user's authenticated SSH connection.",
                ),
        )
        .with_instructions(include_str!("../docs/agent-connection.md"))
    }

    fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListPromptsResult, rmcp::ErrorData>> + Send + '_
    {
        let prompts = vec![
            Prompt::new(
                "slurm-safety",
                Some("Safety guidance for Slurm job submission"),
                Some(vec![
                    PromptArgument::new("cluster")
                        .with_description(
                            "Cluster id from the config; omit for the default cluster.",
                        )
                        .with_required(false),
                ]),
            )
            .with_title("Slurm safety guidance"),
        ];
        std::future::ready(Ok(ListPromptsResult::with_all_items(prompts)))
    }

    fn get_prompt(
        &self,
        request: rmcp::model::GetPromptRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetPromptResponse, rmcp::ErrorData>> + Send + '_
    {
        let cluster = match request.arguments.and_then(|args| {
            args.get("cluster")
                .and_then(|v| v.as_str())
                .map(String::from)
        }) {
            Some(cluster_id) => self.acquire_cluster(normalize_cluster_id(Some(&cluster_id))),
            None => self.acquire_cluster(None),
        };
        let result = match cluster {
            Ok(cluster) => {
                let rendered = render_resources(&cluster);
                let text = rendered
                    .slurm_prompt
                    .or_else(|| Some(rendered.policy.clone()))
                    .unwrap_or_else(|| "No prompt configured.".to_string());
                Ok(
                    GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)])
                        .with_description(format!(
                            "Slurm safety guidance for {}",
                            cluster.display_name()
                        ))
                        .into(),
                )
            }
            Err(err) => Err(to_mcp_error(err)),
        };
        std::future::ready(result)
    }

    fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, rmcp::ErrorData>> + Send + '_
    {
        let snapshot = self.snapshot();
        let resources = snapshot
            .compiled
            .clusters()
            .flat_map(|cluster| build_resource_set(cluster.as_ref()))
            .collect::<Vec<_>>();
        std::future::ready(Ok(ListResourcesResult::with_all_items(resources)))
    }

    fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResponse, rmcp::ErrorData>> + Send + '_
    {
        let uri = request.uri;
        let (cluster_id, kind) = match parse_hpc_uri(&uri) {
            Some(parts) => parts,
            None => {
                return std::future::ready(Err(rmcp::ErrorData::resource_not_found(
                    format!("Unsupported resource URI: {uri}"),
                    None,
                )));
            }
        };
        let cluster = match self.acquire_cluster(Some(&cluster_id)) {
            Ok(cluster) => cluster,
            Err(err) => return std::future::ready(Err(to_mcp_error(err))),
        };
        let rendered = render_resources(&cluster);
        let text = match kind.as_str() {
            "policy" => rendered.policy,
            "filesystem" => rendered.filesystem,
            "partitions" => rendered.partitions,
            _ => {
                return std::future::ready(Err(rmcp::ErrorData::resource_not_found(
                    format!("Unknown resource path '{kind}' in URI '{uri}'"),
                    None,
                )));
            }
        };
        let contents = ResourceContents::text(text, uri);
        std::future::ready(Ok(ReadResourceResult::new(vec![contents]).into()))
    }
}

pub async fn run_stdio(config: ConfigManager, remote_user: String) -> AppResult<()> {
    let server = McpServer::new(config.clone(), remote_user);
    let transport = rmcp::transport::stdio();
    let service = server
        .serve(transport)
        .await
        .map_err(|e| AppError::Io(format!("Failed to start MCP server: {e}")))?;
    service
        .waiting()
        .await
        .map_err(|e| AppError::Io(format!("Server terminated: {e}")))?;
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellExecParams {
    /// Shell command line, run by bash on the cluster login node.
    pub command: String,
    /// Remote working directory; defaults to [defaults].default_cwd. The command
    /// does not run if this directory cannot be entered.
    pub cwd: Option<String>,
    /// Extra environment variables for this command only. Names must be shell
    /// identifiers (letters, digits, underscore; not starting with a digit).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Allocate a pseudo-terminal, only for programs that need one. false keeps
    /// stdout and stderr separate; true merges them into one "pty" stream.
    /// Defaults to [defaults].pty (false).
    pub pty: Option<bool>,
    /// Run under a login shell so profile and module setup is loaded. Defaults
    /// to [defaults].login_shell.
    pub login_shell: Option<bool>,
    /// Seconds before the local SSH client is killed; defaults to
    /// [defaults].exec_timeout_s (120). Does not cancel Slurm jobs the command
    /// already submitted.
    pub timeout_s: Option<u64>,
    /// Cluster id from the config; omit to use the default cluster.
    pub cluster: Option<String>,
}

/// Result of a remote command. A failed call uses the same shape with `ok`
/// false and `error` set, so `isError` results still match the output schema.
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct ExecResponse {
    /// True only when the command ran to completion with exit code 0.
    pub ok: bool,
    /// Remote exit status; absent when the command did not run to completion.
    pub exit_code: Option<i32>,
    #[schemars(with = "i64")]
    pub duration_ms: u64,
    /// Output by stream: "stdout" and "stderr", or one merged "pty" or "tmux"
    /// stream. After a failure this holds whatever was captured before it.
    pub streams: HashMap<String, String>,
    /// True when a stream exceeded [defaults].max_output_bytes; its start and
    /// end are kept around an omission marker.
    pub truncated: bool,
    /// Advisory notes, such as a warning that the command looks like compute
    /// work that belongs in Slurm.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<String>,
    /// Why the call failed; present only on error results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorInfo>,
}

impl ExecResponse {
    fn completed(result: ssh::ExecResult, status: Vec<String>) -> Self {
        Self {
            ok: result.exit_code == 0,
            exit_code: Some(result.exit_code),
            duration_ms: result.duration.as_millis() as u64,
            streams: lossy_streams(result.streams),
            status,
            ..Default::default()
        }
    }

    /// Cap each stream at `limit` bytes, keeping its head and tail.
    fn capped(mut self, limit: usize) -> Self {
        for text in self.streams.values_mut() {
            if let Some(kept) = head_and_tail(text, limit) {
                *text = kept;
                self.truncated = true;
            }
        }
        self
    }
}

/// Returns `text` cut to about `limit` bytes, keeping the first and last
/// halves on char boundaries, or None when it already fits.
fn head_and_tail(text: &str, limit: usize) -> Option<String> {
    if text.len() <= limit {
        return None;
    }
    let head = text.floor_char_boundary(limit / 2);
    let tail = text.ceil_char_boundary(text.len() - limit / 2);
    let omitted = tail - head;
    Some(format!(
        "{}\n[... {omitted} bytes omitted ...]\n{}",
        &text[..head],
        &text[tail..]
    ))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionOpenParams {
    /// New tmux session name: letters, digits, '_' or '-'. Use a distinct name
    /// per task.
    pub name: String,
    /// Starting directory for the session's shell.
    pub cwd: Option<String>,
    /// Cluster id from the config; omit to use the default cluster.
    pub cluster: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionExecParams {
    /// Name of a session previously opened with session_open.
    pub name: String,
    /// Command run by a fresh login Bash child; shell state does not persist
    /// between calls.
    pub command: String,
    /// Working directory for this command; pass it on every call.
    pub cwd: Option<String>,
    /// Environment variables for this command only; pass them on every call.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Seconds before GNU timeout terminates the command's process group.
    /// Defaults to 300; must be greater than zero.
    pub timeout_s: Option<u64>,
    /// Cluster id from the config; omit to use the default cluster.
    pub cluster: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionCloseParams {
    /// Name of a session this client opened with session_open.
    pub name: String,
    /// Cluster id from the config; omit to use the default cluster.
    pub cluster: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClusterScopedParams {
    /// Cluster id from the config; omit to use the default cluster.
    pub cluster: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionResponse {
    pub ok: bool,
    pub message: String,
    /// Why the call failed; present only on error results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorInfo>,
}

impl SessionResponse {
    fn done(message: String) -> Self {
        Self {
            ok: true,
            message,
            error: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct HpcDescribeOutput {
    pub cluster: Cluster,
    pub config_version: u64,
    pub resources: RenderedResources,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HealthCheckResponse {
    pub master_state: String,
    pub control_path: String,
    pub message: Option<String>,
    #[schemars(with = "i64")]
    pub config_version: u64,
    /// Why the call failed; present only on error results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorInfo>,
}

/// Machine-readable reason a tool call failed.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ToolErrorInfo {
    /// Stable code such as InvalidInput, DeniedByPolicy, Needs2FA, Timeout,
    /// SshSpawn, TmuxError, ConfigError or IoError.
    pub code: String,
    pub message: String,
    /// What to do next, when there is specific guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl From<AppError> for ToolErrorInfo {
    fn from(err: AppError) -> Self {
        let hint = match &err {
            AppError::SshTimeout(_) => Some(
                "The command was stopped; any Slurm job it submitted keeps running. Check job state with squeue before retrying.",
            ),
            AppError::DeniedByPolicy(_) => Some(
                "The command matched the safety denylist. Do not rephrase it to evade the check; ask the user how to proceed.",
            ),
            _ => None,
        };
        Self {
            code: err.code().to_string(),
            message: err.to_string(),
            hint: hint.map(String::from),
        }
    }
}

/// Tool responses that can describe a failed call in their own schema.
trait ToolOutput: Serialize {
    fn failed(error: ToolErrorInfo) -> Self;
}

impl ToolOutput for ExecResponse {
    fn failed(error: ToolErrorInfo) -> Self {
        Self {
            error: Some(error),
            ..Default::default()
        }
    }
}

impl ToolOutput for SessionResponse {
    fn failed(error: ToolErrorInfo) -> Self {
        Self {
            ok: false,
            message: error.message.clone(),
            error: Some(error),
        }
    }
}

impl ToolOutput for ToolErrorInfo {
    fn failed(error: ToolErrorInfo) -> Self {
        error
    }
}

impl ToolOutput for HealthCheckResponse {
    fn failed(error: ToolErrorInfo) -> Self {
        Self {
            master_state: "Unknown".into(),
            control_path: String::new(),
            message: Some(error.message.clone()),
            config_version: 0,
            error: Some(error),
        }
    }
}

/// A tool execution failure. It reaches the model as a `CallToolResult` with
/// `isError: true` and structured content, not as a JSON-RPC error (SEP-1303);
/// JSON-RPC errors stay reserved for malformed requests.
pub struct ToolFailure<T>(T);

impl<T: ToolOutput> From<ToolErrorInfo> for ToolFailure<T> {
    fn from(error: ToolErrorInfo) -> Self {
        Self(T::failed(error))
    }
}

impl<T: ToolOutput> From<AppError> for ToolFailure<T> {
    fn from(err: AppError) -> Self {
        ToolErrorInfo::from(err).into()
    }
}

impl From<ExecFailure> for ExecResponse {
    fn from(failure: ExecFailure) -> Self {
        Self {
            duration_ms: failure.duration.as_millis() as u64,
            streams: lossy_streams(failure.streams),
            ..Self::failed(failure.error.into())
        }
    }
}

impl From<SessionExecFailure> for ExecResponse {
    fn from(failure: SessionExecFailure) -> Self {
        Self {
            duration_ms: failure.duration.as_millis() as u64,
            streams: HashMap::from([("tmux".to_string(), failure.output)]),
            ..Self::failed(failure.error.into())
        }
    }
}

impl<T: Serialize> IntoCallToolResult for ToolFailure<T> {
    fn into_call_tool_result(self) -> Result<CallToolResponse, rmcp::ErrorData> {
        let value = serde_json::to_value(self.0).map_err(|e| {
            rmcp::ErrorData::internal_error(format!("Failed to serialize tool error: {e}"), None)
        })?;
        Ok(CallToolResult::structured_error(value).into())
    }
}

/// Sends `notifications/progress` while shell_exec output arrives, when the
/// request carried a progressToken. Throttled to one notification a second;
/// a single forwarding task keeps them in order.
struct OutputProgress {
    tx: mpsc::UnboundedSender<u64>,
    task: JoinHandle<()>,
    bytes: u64,
    last_sent: Option<Instant>,
}

impl OutputProgress {
    fn start(request: &RequestContext<RoleServer>) -> Option<Self> {
        let token = request.meta.get_progress_token()?;
        let peer = request.peer.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
        let task = tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                let param = ProgressNotificationParam::new(token.clone(), bytes as f64)
                    .with_message(format!("{bytes} bytes of output received"));
                let _ = peer.notify_progress(param).await;
            }
        });
        Some(Self {
            tx,
            task,
            bytes: 0,
            last_sent: None,
        })
    }

    fn record(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        self.bytes += len as u64;
        if self
            .last_sent
            .is_none_or(|sent| sent.elapsed() >= Duration::from_secs(1))
        {
            self.last_sent = Some(Instant::now());
            let _ = self.tx.send(self.bytes);
        }
    }

    /// Flush pending notifications so none arrive after the tool result.
    async fn finish(self) {
        drop(self.tx);
        let _ = self.task.await;
    }
}

fn lossy_streams(streams: HashMap<String, Vec<u8>>) -> HashMap<String, String> {
    streams
        .into_iter()
        .map(|(name, data)| (name, String::from_utf8_lossy(&data).into_owned()))
        .collect()
}

/// Protocol-level error for prompt and resource requests.
fn to_mcp_error(err: AppError) -> rmcp::ErrorData {
    let code = match err {
        AppError::InvalidInput(_) | AppError::Config(_) => rmcp::model::ErrorCode::INVALID_PARAMS,
        _ => rmcp::model::ErrorCode::INTERNAL_ERROR,
    };
    rmcp::ErrorData::new(code, err.to_string(), Some(json!({ "code": err.code() })))
}

async fn ensure_master(context: &SshContext, remote_user: &str) -> Result<(), ToolErrorInfo> {
    match ssh::check_master(context).await? {
        MasterState::Running => Ok(()),
        MasterState::Missing | MasterState::Stale => Err(needs_2fa_error(context, remote_user)),
    }
}

fn needs_2fa_error(context: &SshContext, remote_user: &str) -> ToolErrorInfo {
    let cluster_id = &context.cluster.id;
    ToolErrorInfo {
        code: "Needs2FA".into(),
        message: format!(
            "Cluster connection is unavailable for '{cluster_id}'. Tell the user to check GW VPN if off campus and reconnect in their own terminal. Never guess or submit 2FA codes."
        ),
        hint: Some(format!(
            "Ask the user to run: ghm up --cluster {cluster_id} --user {remote_user}. They must complete authentication themselves; do not paste codes into chat."
        )),
    }
}

fn build_resource_set(cluster: &Cluster) -> Vec<Resource> {
    let mut resources = Vec::new();
    let policy_uri = format!("hpc://{}/policy", cluster.id);
    resources.push(make_resource(
        &policy_uri,
        &format!("{} policy", cluster.display_name()),
        "Site policy guidance for submission and filesystem usage.",
    ));
    let filesystem_uri = format!("hpc://{}/filesystem", cluster.id);
    resources.push(make_resource(
        &filesystem_uri,
        &format!("{} filesystem", cluster.display_name()),
        "Filesystem usage guidance.",
    ));
    let partitions_uri = format!("hpc://{}/partitions", cluster.id);
    resources.push(make_resource(
        &partitions_uri,
        &format!("{} partitions", cluster.display_name()),
        "Known Slurm partitions and notes.",
    ));
    resources
}

fn make_resource(uri: &str, name: &str, description: &str) -> Resource {
    Resource::new(uri, name)
        .with_description(description)
        .with_mime_type("text/plain")
}

fn parse_hpc_uri(uri: &str) -> Option<(String, String)> {
    const PREFIX: &str = "hpc://";
    if !uri.starts_with(PREFIX) {
        return None;
    }
    let rest = &uri[PREFIX.len()..];
    let mut parts = rest.splitn(2, '/');
    let cluster = parts.next()?.to_string();
    let path = parts.next().unwrap_or("").to_string();
    Some((cluster, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_failure_becomes_structured_error_result() {
        let failure = ExecFailure {
            error: AppError::SshTimeout(Duration::from_secs(1)),
            duration: Duration::from_millis(1500),
            streams: HashMap::from([("stdout".to_string(), b"partial".to_vec())]),
            tail: "partial".into(),
        };
        let response = ToolFailure(ExecResponse::from(failure))
            .into_call_tool_result()
            .expect("tool failures are results, not protocol errors");
        let CallToolResponse::Complete(result) = response else {
            panic!("expected a complete result");
        };
        assert_eq!(result.is_error, Some(true));
        let value = result.structured_content.expect("structured content");
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "Timeout");
        assert_eq!(value["streams"]["stdout"], "partial");
        assert!(value["exit_code"].is_null());
    }

    #[test]
    fn capped_streams_keep_head_and_tail() {
        let text = format!("{}{}{}", "a".repeat(100), "中".repeat(100), "z".repeat(100));
        let response = ExecResponse {
            streams: HashMap::from([
                ("stdout".to_string(), text),
                ("stderr".to_string(), "short".to_string()),
            ]),
            ..Default::default()
        }
        .capped(64);
        assert!(response.truncated);
        let stdout = &response.streams["stdout"];
        assert!(stdout.starts_with(&"a".repeat(32)));
        assert!(stdout.ends_with(&"z".repeat(32)));
        assert!(stdout.contains("bytes omitted"));
        assert_eq!(response.streams["stderr"], "short");
        assert!(!ExecResponse::default().capped(64).truncated);
    }

    #[test]
    fn parse_hpc_uri_extracts_cluster_and_path() {
        let parsed = parse_hpc_uri("hpc://pegasus/policy").expect("uri parsed");
        assert_eq!(parsed.0, "pegasus");
        assert_eq!(parsed.1, "policy");
    }

    #[test]
    fn parse_hpc_uri_handles_missing_path() {
        let parsed = parse_hpc_uri("hpc://pegasus").expect("uri parsed");
        assert_eq!(parsed.1, "");
    }

    #[test]
    fn parse_hpc_uri_rejects_other_schemes() {
        assert!(parse_hpc_uri("file://tmp").is_none());
    }
}
