/// Errors produced by tool implementations. Tool failures are data: the agent
/// loop turns them into `role=tool` messages instead of aborting the turn.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("path escapes workspace: {0}")]
    PathEscape(String),
    #[error("{0}")]
    Denied(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type ToolResult<T> = Result<T, ToolError>;
