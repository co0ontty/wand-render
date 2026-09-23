//! 库侧错误类型。守护进程按 [`RenderErrorKind`] 映射成协议错误码，
//! 不靠字符串匹配判断语义。

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderErrorKind {
  NotFound,
  BadRequest,
  Conflict,
  Internal,
}

#[derive(Debug, Error)]
pub enum RenderError {
  #[error("session {0} not found")]
  NotFound(String),
  #[error("{0}")]
  BadRequest(String),
  #[error("{0}")]
  Conflict(String),
  #[error("pty io error: {0}")]
  Io(#[from] std::io::Error),
  #[error("spawn failed: {0}")]
  Spawn(String),
  #[error("{0}")]
  Internal(String),
}

impl RenderError {
  pub fn kind(&self) -> RenderErrorKind {
    match self {
      RenderError::NotFound(_) => RenderErrorKind::NotFound,
      RenderError::BadRequest(_) => RenderErrorKind::BadRequest,
      RenderError::Conflict(_) => RenderErrorKind::Conflict,
      RenderError::Io(_) | RenderError::Spawn(_) | RenderError::Internal(_) => {
        RenderErrorKind::Internal
      }
    }
  }
}
