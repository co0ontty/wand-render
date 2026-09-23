//! PTY 所有权、输出 journal、VT 屏幕模型与快照序列化。
//!
//! 契约见 `docs/render-protocol.md`：本 crate 是「Render 常驻进程」的内核，
//! 只做终端与进程，不碰 DB / HTTP / 聊天投影。守护进程 `wand-renderd`
//! 负责 socket、鉴权与方法分发，它只通过 [`registry::RenderRegistry`] 与本 crate 交互。

pub mod bounds;
pub mod error;
pub mod marker;
pub mod paths;
pub mod registry;
pub mod resources;
/// 连接方身份与 socket 归属校验依赖 `getpeereid` / `SO_PEERCRED` / 文件 uid，
/// 这些只在 Unix 上存在（协议 §9.5.2）。
#[cfg(unix)]
pub mod security;
pub mod session;
pub mod signal;
pub mod sink;
pub mod snapshot;
pub mod time;
pub mod utf8;

pub use error::{RenderError, RenderErrorKind};
pub use paths::{check_platform_supported, render_paths, RenderPaths};
pub use registry::RenderRegistry;
pub use session::Session;
pub use sink::EventSink;
