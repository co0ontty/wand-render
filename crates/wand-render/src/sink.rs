//! 事件出口。Render 内核不认识 socket、也不知道有多少客户端在线：
//! 它只把协议事件交给一个 [`EventSink`]，由守护进程决定怎么广播。

use wand_render_protocol::Event;

pub trait EventSink: Send + Sync {
  fn publish(&self, event: Event);
}

/// 丢弃所有事件（测试与工具场景）。
pub struct NullSink;

impl EventSink for NullSink {
  fn publish(&self, _event: Event) {}
}
