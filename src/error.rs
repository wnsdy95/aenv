use thiserror::Error;

#[derive(Debug, Error)]
pub enum AenvError {
    #[error("env '{0}' not found")]
    EnvNotFound(String),

    #[error("env '{0}' already exists")]
    EnvAlreadyExists(String),

    #[error("invalid env name '{0}': must be [a-zA-Z0-9._-]+ and not start with '.'")]
    InvalidEnvName(String),

    #[error("real claude binary not found in PATH (after excluding aenv shim dir)")]
    RealClaudeNotFound,
}
