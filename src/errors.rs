use std::time::Duration;

use thiserror::Error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error(
        "Cluster connection is not running; the user must run {0} in their own terminal and complete 2FA"
    )]
    Needs2fa(String),
    #[allow(dead_code)]
    #[error("ControlMaster not running")]
    MasterDown,
    #[allow(dead_code)]
    #[error("Stale ControlMaster socket detected")]
    MasterStale,
    #[error("SSH spawn failed: {0}")]
    SshSpawn(String),
    #[error("SSH exec timed out after {0:?}")]
    SshTimeout(Duration),
    #[error("Command denied by policy: {0}")]
    DeniedByPolicy(String),
    #[error("tmux error: {0}")]
    Tmux(String),
    #[allow(dead_code)]
    #[error("Unknown session: {0}")]
    UnknownSession(String),
    #[error("Config error: {0}")]
    Config(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    #[error("Request cancelled by the client")]
    Cancelled,
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Needs2fa(_) => "Needs2FA",
            AppError::MasterDown => "MasterDown",
            AppError::MasterStale => "MasterStale",
            AppError::SshSpawn(_) => "SshSpawn",
            AppError::SshTimeout(_) => "Timeout",
            AppError::DeniedByPolicy(_) => "DeniedByPolicy",
            AppError::Tmux(_) => "TmuxError",
            AppError::UnknownSession(_) => "UnknownSession",
            AppError::Config(_) => "ConfigError",
            AppError::Io(_) => "IoError",
            AppError::InvalidInput(_) => "InvalidInput",
            AppError::Cancelled => "Cancelled",
        }
    }
}
