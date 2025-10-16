use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    Json, ServiceExt,
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{
        GetPromptResult, ListPromptsResult, ListResourcesResult, LoggingLevel,
        LoggingMessageNotificationParam, Prompt, PromptMessage, PromptMessageRole,
        ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Semaphore;

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

    fn to_shell_response(&self, result: ssh::ExecResult, status: Vec<String>) -> ShellExecResponse {
        let streams = result
            .streams
            .into_iter()
            .map(|(name, data)| (name, String::from_utf8_lossy(&data).to_string()))
            .collect();
        ShellExecResponse {
            ok: true,
            exit_code: result.exit_code,
            duration_ms: result.duration.as_millis() as u64,
            streams,
            tail: result.tail,
            status,
        }
    }
}

#[tool_router]
impl McpServer {
    #[tool(
        name = "shell_exec",
        description = "Execute a command on the remote cluster via SSH ControlMaster."
    )]
    pub async fn shell_exec(
        &self,
        Parameters(params): Parameters<ShellExecParams>,
        peer: rmcp::service::Peer<rmcp::RoleServer>,
    ) -> Result<Json<ShellExecResponse>, rmcp::ErrorData> {
        let snapshot = self.snapshot();
        let cluster = self
            .acquire_cluster(params.cluster.as_deref())
            .map_err(to_mcp_error)?;
        let context = self.build_context(cluster.clone());
        let permit = self
            .concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| {
                rmcp::ErrorData::internal_error(
                    "Failed to acquire execution permit",
                    Some(json!({ "error": e.to_string() })),
                )
            })?;
        let master_state = ssh::check_master(&context).await.map_err(to_mcp_error)?;
        match master_state {
            MasterState::Running => {}
            MasterState::Missing | MasterState::Stale => {
                drop(permit);
                return Err(needs_2fa_error(&context, &self.remote_user));
            }
        }

        if snapshot
            .compiled
            .heuristics
            .denylist
            .iter()
            .any(|re| re.is_match(&params.command))
        {
            drop(permit);
            return Err(to_mcp_error(AppError::DeniedByPolicy(params.command)));
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
            timeout: params.timeout_s.map(Duration::from_secs),
        };
        let cluster_id = cluster.id.clone();
        let log_peer = peer.clone();
        let mut log_closure = move |stream: &str, chunk: &[u8]| {
            if chunk.is_empty() {
                return;
            }
            let text = String::from_utf8_lossy(chunk).to_string();
            let logger_name = format!("shell_exec.{stream}");
            let data = json!({
                "cluster": cluster_id,
                "stream": stream,
                "text": text,
            });
            let peer_clone = log_peer.clone();
            tokio::spawn(async move {
                let _ = peer_clone
                    .notify_logging_message(LoggingMessageNotificationParam {
                        level: LoggingLevel::Info,
                        logger: Some(logger_name),
                        data,
                    })
                    .await;
            });
        };

        let exec_result = ssh::exec(&context, exec_request, &mut log_closure).await;
        drop(permit);
        match exec_result {
            Ok(result) => Ok(Json(self.to_shell_response(result, status))),
            Err(failure) => Err(exec_failure_to_error("shell_exec", failure)),
        }
    }

    #[tool(
        name = "session_open",
        description = "Open a tmux-backed session on the cluster."
    )]
    pub async fn session_open(
        &self,
        Parameters(params): Parameters<SessionOpenParams>,
    ) -> Result<Json<SessionResponse>, rmcp::ErrorData> {
        let cluster = self
            .acquire_cluster(params.cluster.as_deref())
            .map_err(to_mcp_error)?;
        let context = self.build_context(cluster.clone());
        ensure_master(&context, &self.remote_user).await?;
        let manager = SessionManager::new(context);
        manager
            .open(&params.name, params.cwd.as_deref())
            .await
            .map_err(to_mcp_error)?;
        Ok(Json(SessionResponse {
            ok: true,
            message: format!(
                "Session '{}' opened on {}",
                params.name,
                cluster.display_name()
            ),
        }))
    }

    #[tool(
        name = "session_exec",
        description = "Execute a command inside an existing tmux session."
    )]
    pub async fn session_exec(
        &self,
        Parameters(params): Parameters<SessionExecParams>,
        peer: rmcp::service::Peer<rmcp::RoleServer>,
    ) -> Result<Json<SessionExecResponse>, rmcp::ErrorData> {
        let cluster = self
            .acquire_cluster(params.cluster.as_deref())
            .map_err(to_mcp_error)?;
        let context = self.build_context(cluster.clone());
        ensure_master(&context, &self.remote_user).await?;
        let manager = SessionManager::new(context);
        let cluster_id = cluster.id.clone();
        let log_peer = peer.clone();
        match manager
            .exec(
                &params.name,
                &params.command,
                params.timeout_s.map(Duration::from_secs),
                |chunk| {
                    if chunk.is_empty() {
                        return Ok(());
                    }
                    let text = chunk.to_string();
                    let data = json!({
                        "cluster": cluster_id.clone(),
                        "stream": "tmux",
                        "text": text,
                    });
                    let logger = format!("session_exec.{}", params.name);
                    let peer_clone = log_peer.clone();
                    tokio::spawn(async move {
                        let _ = peer_clone
                            .notify_logging_message(LoggingMessageNotificationParam {
                                level: LoggingLevel::Info,
                                logger: Some(logger),
                                data,
                            })
                            .await;
                    });
                    Ok(())
                },
            )
            .await
        {
            Ok(result) => {
                let response = SessionExecResponse {
                    ok: true,
                    exit_code: result.exit_code,
                    duration_ms: result.duration.as_millis() as u64,
                    streams: HashMap::from([("tmux".to_string(), result.output)]),
                    tail: result.tail,
                };
                Ok(Json(response))
            }
            Err(failure) => Err(session_failure_to_error("session_exec", failure)),
        }
    }

    #[tool(
        name = "session_close",
        description = "Close a tmux session on the cluster."
    )]
    pub async fn session_close(
        &self,
        Parameters(params): Parameters<SessionCloseParams>,
    ) -> Result<Json<SessionResponse>, rmcp::ErrorData> {
        let cluster = self
            .acquire_cluster(params.cluster.as_deref())
            .map_err(to_mcp_error)?;
        let context = self.build_context(cluster.clone());
        ensure_master(&context, &self.remote_user).await?;
        let manager = SessionManager::new(context);
        manager.close(&params.name).await.map_err(to_mcp_error)?;
        Ok(Json(SessionResponse {
            ok: true,
            message: format!(
                "Session '{}' closed on {}",
                params.name,
                cluster.display_name()
            ),
        }))
    }

    #[tool(
        name = "hpc_describe",
        description = "Describe cluster configuration and policy context."
    )]
    pub async fn hpc_describe(
        &self,
        Parameters(params): Parameters<ClusterScopedParams>,
    ) -> Result<Json<DescribeResponse>, rmcp::ErrorData> {
        let snapshot = self.snapshot();
        let cluster = self
            .acquire_cluster(params.cluster.as_deref())
            .map_err(to_mcp_error)?;
        let resources = render_resources(&cluster);
        Ok(Json(DescribeResponse {
            cluster: (*cluster).clone(),
            config_version: snapshot.version,
            resources,
        }))
    }

    #[tool(
        name = "health_check",
        description = "Report ControlMaster status for a cluster."
    )]
    pub async fn health_check(
        &self,
        Parameters(params): Parameters<ClusterScopedParams>,
    ) -> Result<Json<HealthCheckResponse>, rmcp::ErrorData> {
        let snapshot = self.snapshot();
        let cluster = self
            .acquire_cluster(params.cluster.as_deref())
            .map_err(to_mcp_error)?;
        let context = self.build_context(cluster.clone());
        let control_path = context.control_path().map_err(to_mcp_error)?;
        let state = ssh::check_master(&context).await.map_err(to_mcp_error)?;
        let (state_str, message) = match state {
            MasterState::Running => ("Running".to_string(), None),
            MasterState::Missing => (
                "Missing".to_string(),
                Some(format!(
                    "ControlMaster not established. Run: gwu-hpc-mcp up --cluster {} --user {}",
                    cluster.id, self.remote_user
                )),
            ),
            MasterState::Stale => (
                "Stale".to_string(),
                Some("Existing ControlMaster socket appears stale; run 'gwu-hpc-mcp down' then 'up'.".to_string()),
            ),
        };
        Ok(Json(HealthCheckResponse {
            master_state: state_str,
            control_path: control_path.to_str().unwrap_or_default().to_string(),
            message,
            config_version: snapshot.version,
        }))
    }
}

#[tool_handler]
impl rmcp::ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "GWU Pegasus MCP server. Establish ControlMaster via `gwu-hpc-mcp up` before use."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_logging()
                .build(),
            ..Default::default()
        }
    }

    fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParam>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListPromptsResult, rmcp::ErrorData>> + Send + '_
    {
        let prompts = vec![Prompt::new(
            "slurm-safety",
            Some("Safety guidance for Slurm job submission".to_string()),
            None,
        )];
        std::future::ready(Ok(ListPromptsResult::with_all_items(prompts)))
    }

    fn get_prompt(
        &self,
        request: rmcp::model::GetPromptRequestParam,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetPromptResult, rmcp::ErrorData>> + Send + '_
    {
        let cluster = match request.arguments.and_then(|args| {
            args.get("cluster")
                .and_then(|v| v.as_str())
                .map(String::from)
        }) {
            Some(cluster_id) => self.acquire_cluster(Some(&cluster_id)),
            None => self.acquire_cluster(None),
        };
        let result = match cluster {
            Ok(cluster) => {
                let rendered = render_resources(&cluster);
                let text = rendered
                    .slurm_prompt
                    .or_else(|| Some(rendered.policy.clone()))
                    .unwrap_or_else(|| "No prompt configured.".to_string());
                Ok(GetPromptResult {
                    description: Some(format!(
                        "Slurm safety guidance for {}",
                        cluster.display_name()
                    )),
                    messages: vec![PromptMessage::new_text(PromptMessageRole::Assistant, text)],
                })
            }
            Err(err) => Err(to_mcp_error(err)),
        };
        std::future::ready(result)
    }

    fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParam>,
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
        request: rmcp::model::ReadResourceRequestParam,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::ReadResourceResult, rmcp::ErrorData>>
    + Send
    + '_ {
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
        std::future::ready(Ok(ReadResourceResult {
            contents: vec![contents],
        }))
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
    pub command: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub pty: Option<bool>,
    pub login_shell: Option<bool>,
    pub timeout_s: Option<u64>,
    pub cluster: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ShellExecResponse {
    pub ok: bool,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub streams: HashMap<String, String>,
    pub tail: String,
    pub status: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionOpenParams {
    pub name: String,
    pub cwd: Option<String>,
    pub cluster: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionExecParams {
    pub name: String,
    pub command: String,
    pub timeout_s: Option<u64>,
    pub cluster: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionCloseParams {
    pub name: String,
    pub cluster: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClusterScopedParams {
    pub cluster: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionResponse {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionExecResponse {
    pub ok: bool,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub streams: HashMap<String, String>,
    pub tail: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DescribeResponse {
    pub cluster: Cluster,
    pub config_version: u64,
    pub resources: RenderedResources,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HealthCheckResponse {
    pub master_state: String,
    pub control_path: String,
    pub message: Option<String>,
    pub config_version: u64,
}

fn to_mcp_error(err: AppError) -> rmcp::ErrorData {
    error_with_details(err, None)
}

fn error_with_details(err: AppError, extra: Option<serde_json::Value>) -> rmcp::ErrorData {
    let code = match err {
        AppError::InvalidInput(_)
        | AppError::DeniedByPolicy(_)
        | AppError::Needs2fa(_)
        | AppError::MasterDown
        | AppError::MasterStale
        | AppError::UnknownSession(_) => rmcp::model::ErrorCode::INVALID_PARAMS,
        AppError::Config(_) => rmcp::model::ErrorCode::INTERNAL_ERROR,
        AppError::SshTimeout(_) | AppError::SshSpawn(_) | AppError::Tmux(_) | AppError::Io(_) => {
            rmcp::model::ErrorCode::INTERNAL_ERROR
        }
    };

    let data = json!({
        "code": err.code(),
        "details": extra.unwrap_or_else(|| json!(null))
    });

    rmcp::ErrorData::new(code, err.to_string(), Some(data))
}

fn exec_failure_to_error(op: &str, failure: ExecFailure) -> rmcp::ErrorData {
    let streams = failure
        .streams
        .into_iter()
        .map(|(name, data)| (name, String::from_utf8_lossy(&data).to_string()))
        .collect::<HashMap<_, _>>();
    let details = json!({
        "operation": op,
        "duration_ms": failure.duration.as_millis() as u64,
        "streams": streams,
        "tail": failure.tail,
    });
    error_with_details(failure.error, Some(details))
}

fn session_failure_to_error(op: &str, failure: SessionExecFailure) -> rmcp::ErrorData {
    let details = json!({
        "operation": op,
        "duration_ms": failure.duration.as_millis() as u64,
        "output": failure.output,
        "tail": failure.tail,
    });
    error_with_details(failure.error, Some(details))
}

async fn ensure_master(context: &SshContext, remote_user: &str) -> Result<(), rmcp::ErrorData> {
    let state = ssh::check_master(context).await.map_err(to_mcp_error)?;
    match state {
        MasterState::Running => Ok(()),
        MasterState::Missing | MasterState::Stale => Err(needs_2fa_error(context, remote_user)),
    }
}

fn needs_2fa_error(context: &SshContext, remote_user: &str) -> rmcp::ErrorData {
    let cluster_id = &context.cluster.id;
    let message =
        format!("ControlMaster is not active for cluster '{cluster_id}'. Run 2FA to establish it.");
    let hint = format!("Run: gwu-hpc-mcp up --cluster {cluster_id} --user {remote_user}");
    let data = json!({
        "cluster": cluster_id,
        "hint": hint,
        "control_path": context.control_path().ok().and_then(|p| p.to_str().map(|s| s.to_string()))
    });
    rmcp::ErrorData::new(
        rmcp::model::ErrorCode::INVALID_PARAMS,
        message,
        Some(json!({
            "code": "Needs2FA",
            "details": data
        })),
    )
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
    let mut raw = rmcp::model::RawResource::new(uri.to_string(), name.to_string());
    raw.description = Some(description.to_string());
    raw.mime_type = Some("text/plain".to_string());
    rmcp::model::Annotated::new(raw, None)
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
    use super::parse_hpc_uri;

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
