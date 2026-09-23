//! 真实 PTY 上的端到端验证（`docs/render-protocol.md` §4/§6）。
//!
//! 这些测试直接跑 `/bin/sh`，覆盖 spawn / write / resize / kill / seq / 有界窗口 /
//! UTF-8 跨 chunk / 标记剥离与 createOrAttach 语义。

#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wand_render::{EventSink, RenderError, RenderRegistry};
use wand_render_protocol::{
  CreateOrAttachParams, Event, SessionStatus, ShutdownMode, PTY_OUTPUT_MAX_CHARS,
};
use wand_render::snapshot::LIST_SNAPSHOT_MAX_BYTES;

const SHELL: &str = "/bin/sh";
const AWK: &str = "/usr/bin/awk";
const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct RecordingSink {
  events: Mutex<Vec<Event>>,
}

impl EventSink for RecordingSink {
  fn publish(&self, event: Event) {
    self.events
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(event);
  }
}

impl RecordingSink {
  fn snapshot(&self) -> Vec<Event> {
    self.events
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  fn data_text(&self, session_id: &str) -> String {
    self
      .snapshot()
      .iter()
      .filter_map(|event| match event {
        Event::Data {
          session_id: id,
          data,
          ..
        } if id == session_id => Some(data.as_str()),
        _ => None,
      })
      .collect()
  }

  fn data_seqs(&self, session_id: &str) -> Vec<u64> {
    self
      .snapshot()
      .iter()
      .filter_map(|event| match event {
        Event::Data {
          session_id: id, seq, ..
        } if id == session_id => Some(*seq),
        _ => None,
      })
      .collect()
  }

  fn exit_of(&self, session_id: &str) -> Option<(Option<i32>, Option<i32>)> {
    self.snapshot().iter().find_map(|event| match event {
      Event::Exit {
        session_id: id,
        exit_code,
        signal,
        ..
      } if id == session_id => Some((*exit_code, *signal)),
      _ => None,
    })
  }

  fn last_data_index(&self, session_id: &str) -> Option<usize> {
    self.snapshot().iter().rposition(|event| {
      matches!(event, Event::Data { session_id: id, .. } if id == session_id)
    })
  }

  fn exit_index(&self, session_id: &str) -> Option<usize> {
    self.snapshot().iter().position(|event| {
      matches!(event, Event::Exit { session_id: id, .. } if id == session_id)
    })
  }
}

fn env_map() -> BTreeMap<String, String> {
  let mut env = BTreeMap::new();
  env.insert("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into());
  env.insert("HOME".into(), std::env::temp_dir().to_string_lossy().to_string());
  env.insert("LANG".into(), "C.UTF-8".into());
  env
}

fn params(session_id: &str, args: &[&str], cols: u16, rows: u16) -> CreateOrAttachParams {
  CreateOrAttachParams {
    session_id: session_id.to_string(),
    file: SHELL.to_string(),
    args: args.iter().map(|arg| arg.to_string()).collect(),
    cwd: std::env::temp_dir().to_string_lossy().to_string(),
    env: env_map(),
    name: "xterm-256color".to_string(),
    cols,
    rows,
    launch_marker_token: None,
    after_seq: 0,
  }
}

fn registry() -> (Arc<RenderRegistry>, Arc<RecordingSink>) {
  let sink = Arc::new(RecordingSink::default());
  let registry = RenderRegistry::new("test", sink.clone());
  (registry, sink)
}

fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut condition: F) -> bool {
  let deadline = Instant::now() + timeout;
  loop {
    if condition() {
      return true;
    }
    if Instant::now() >= deadline {
      return false;
    }
    std::thread::sleep(Duration::from_millis(10));
  }
}

fn wait_for_exit(sink: &RecordingSink, session_id: &str) -> (Option<i32>, Option<i32>) {
  let ok = wait_until(WAIT, || sink.exit_of(session_id).is_some());
  assert!(ok, "session {session_id} never reported exit");
  sink.exit_of(session_id).expect("exit event")
}

/// spawn `/bin/sh -c "printf hello"`：先收到含 hello 的 data，再收到 exitCode 0 的 exit。
#[test]
fn spawn_emits_data_then_exit_code_zero() {
  let (registry, sink) = registry();
  let created = registry
    .create_or_attach(&params("plain", &["-c", "printf hello"], 80, 24))
    .expect("create");
  assert!(created.is_new);
  assert_eq!(created.state.status, SessionStatus::Running);
  assert!(created.state.pid > 0);
  assert!(!created.state.incarnation_id.is_empty());
  assert_eq!(created.state.cols, 80);
  assert_eq!(created.state.rows, 24);
  assert_eq!(created.state.launch_marker_token, None);
  assert!(created.state.terminal_snapshot.is_some());

  let (exit_code, signal) = wait_for_exit(&sink, "plain");
  assert_eq!(exit_code, Some(0));
  assert_eq!(signal, None);
  assert!(sink.data_text("plain").contains("hello"));

  let last_data = sink.last_data_index("plain").expect("data before exit");
  let exit = sink.exit_index("plain").expect("exit index");
  assert!(last_data < exit, "exit event must follow the last data event");
  registry.forget("plain");
}

/// write 回显：`/bin/sh` 里写 "echo hi\r"，输出包含 hi。
#[test]
fn write_round_trips_through_an_interactive_shell() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params("echo", &[], 80, 24))
    .expect("create");
  registry.write("echo", "echo hi\r").expect("write");
  assert!(
    wait_until(WAIT, || sink.data_text("echo").contains("hi")),
    "output never echoed: {:?}",
    sink.data_text("echo")
  );
  // 真正执行过的证据：算术展开的结果只可能来自子进程。
  registry.write("echo", "echo $((6*7))\r").expect("write");
  assert!(
    wait_until(WAIT, || sink.data_text("echo").contains("42")),
    "command was never executed: {:?}",
    sink.data_text("echo")
  );
  registry.forget("echo");
}

/// resize：cols=80 rows=24 启动的 `stty size` 输出 "24 80"。
#[test]
fn spawn_applies_the_requested_window_size() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params("size", &["-c", "stty size; sleep 0.3"], 80, 24))
    .expect("create");
  assert!(
    wait_until(WAIT, || sink.data_text("size").contains("24 80")),
    "unexpected stty output: {:?}",
    sink.data_text("size")
  );
  registry.forget("size");
}

/// resize 真实生效：80x24 启动后改成 100x40，子进程读到 40 100。
#[test]
fn resize_is_applied_to_the_child() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params("resize", &["-c", "sleep 0.5; stty size"], 80, 24))
    .expect("create");
  std::thread::sleep(Duration::from_millis(100));
  registry.resize("resize", 100, 40).expect("resize");
  let state = registry.attach("resize", 0).expect("state");
  assert_eq!((state.cols, state.rows), (100, 40));
  assert!(
    wait_until(WAIT, || sink.data_text("resize").contains("40 100")),
    "resize never reached the child: {:?}",
    sink.data_text("resize")
  );
  registry.forget("resize");
}

/// seq 单调，`attach(afterSeq)` 只返回 seq > afterSeq 的补洞数据。
#[test]
fn seq_is_monotonic_and_attach_filters_by_after_seq() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params(
      "seq",
      &["-c", "printf a; sleep 0.05; printf b; sleep 0.05; printf c"],
      80,
      24,
    ))
    .expect("create");
  wait_for_exit(&sink, "seq");
  let seqs = sink.data_seqs("seq");
  assert!(seqs.len() >= 3, "expected several chunks, got {seqs:?}");
  assert!(
    seqs.windows(2).all(|pair| pair[0] < pair[1]),
    "seq must be strictly increasing: {seqs:?}"
  );
  assert_eq!(seqs[0], 1, "seq starts at 1 per incarnation");

  let all = registry.attach("seq", 0).expect("attach");
  assert_eq!(all.seq, *seqs.last().unwrap());
  assert_eq!(all.chunks.len(), seqs.len());

  let last_seq = *seqs.last().unwrap();
  let tail = registry.attach("seq", last_seq - 1).expect("attach tail");
  assert_eq!(tail.chunks.len(), 1);
  assert_eq!(tail.chunks[0].seq, last_seq);
  assert_eq!(tail.seq, last_seq);

  let none = registry.attach("seq", last_seq).expect("attach none");
  assert!(none.chunks.is_empty());
  // output 不受 afterSeq 影响（Server 需要完整文本）
  assert!(none.output.contains('c'));
  registry.forget("seq");
}

/// 30 万字符灌进去后 chunks 累计 ≤20 万，且保留的是最新内容。
#[test]
fn journal_window_keeps_only_the_newest_output() {
  let (registry, sink) = registry();
  let script = "awk 'BEGIN{for(i=0;i<50000;i++)printf \"%06d\", i}'";
  registry
    .create_or_attach(&params("volume", &["-c", script], 80, 24))
    .expect("create");
  wait_for_exit(&sink, "volume");

  let state = registry.attach("volume", 0).expect("attach");
  let joined: String = state.chunks.iter().map(|chunk| chunk.data.as_str()).collect();
  assert!(
    joined.chars().count() <= PTY_OUTPUT_MAX_CHARS,
    "chunks window grew to {} chars",
    joined.chars().count()
  );
  assert!(joined.ends_with("049999"), "window tail is stale");
  assert!(
    state.output.chars().count() <= PTY_OUTPUT_MAX_CHARS,
    "output grew to {} chars",
    state.output.chars().count()
  );
  assert!(state.output.ends_with("049999"), "output tail is stale");
  assert!(
    !state.output.contains("0000000"),
    "output kept the oldest content"
  );
  registry.forget("volume");
}

/// 多字节标量跨 chunk 切分：最终文本正确且不含 U+FFFD。
#[test]
fn multibyte_output_is_never_corrupted() {
  let (registry, sink) = registry();
  let script = "awk 'BEGIN{for(i=0;i<2000;i++)printf \"中文🙂\"}'";
  registry
    .create_or_attach(&params("multibyte", &["-c", script], 80, 24))
    .expect("create");
  wait_for_exit(&sink, "multibyte");

  let expected = "中文🙂".repeat(2000);
  let state = registry.attach("multibyte", 0).expect("attach");
  assert!(!state.output.contains('\u{fffd}'), "replacement character leaked");
  assert_eq!(state.output, expected);
  let joined: String = state.chunks.iter().map(|chunk| chunk.data.as_str()).collect();
  assert_eq!(joined, expected);
  registry.forget("multibyte");
}

/// 私有退出标记：`chunks`/事件保留原始数据，`output` 与屏幕模型必须已剥离。
#[test]
fn launch_marker_is_stripped_from_the_journal_only() {
  let (registry, sink) = registry();
  let mut request = params(
    "marker",
    &[
      "-c",
      r#"printf 'visible\036WAND_CLI_EXIT:tok-1:0\037after'"#,
    ],
    80,
    24,
  );
  request.launch_marker_token = Some("tok-1".to_string());
  registry.create_or_attach(&request).expect("create");
  wait_for_exit(&sink, "marker");

  let state = registry.attach("marker", 0).expect("attach");
  assert_eq!(state.launch_marker_token.as_deref(), Some("tok-1"));
  assert_eq!(state.output, "visibleafter");
  assert!(sink.data_text("marker").contains("WAND_CLI_EXIT:tok-1:0"));
  let snapshot = state.terminal_snapshot.expect("snapshot");
  assert!(!snapshot.data.contains("WAND_CLI_EXIT"));
  registry.forget("marker");
}

/// createOrAttach：已在运行的会话不重启；已退出的会话返回退出状态。
#[test]
fn create_or_attach_never_restarts_a_live_session() {
  let (registry, _sink) = registry();
  let first = registry
    .create_or_attach(&params("live", &[], 80, 24))
    .expect("create");
  let again = registry
    .create_or_attach(&params("live", &["-c", "printf never"], 40, 10))
    .expect("create again");
  assert!(!again.is_new);
  assert_eq!(again.state.incarnation_id, first.state.incarnation_id);
  assert_eq!(again.state.pid, first.state.pid);
  assert_eq!((again.state.cols, again.state.rows), (80, 24));
  registry.forget("live");
}

#[test]
fn create_or_attach_reports_an_exited_session_without_restarting_it() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params("gone", &["-c", "exit 7"], 80, 24))
    .expect("create");
  let (exit_code, signal) = wait_for_exit(&sink, "gone");
  assert_eq!(exit_code, Some(7));
  assert_eq!(signal, None);

  let again = registry
    .create_or_attach(&params("gone", &["-c", "printf restarted"], 80, 24))
    .expect("create again");
  assert!(!again.is_new);
  assert_eq!(again.state.status, SessionStatus::Exited);
  assert_eq!(again.state.exit_code, Some(7));
  assert!(!sink.data_text("gone").contains("restarted"));
  registry.forget("gone");
}

/// kill 支持信号名，并且投递给整个进程组（shell 与它的子进程一起收信号）。
#[test]
fn kill_by_signal_name_terminates_the_child() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params("kill", &["-c", "sleep 30; echo never"], 80, 24))
    .expect("create");
  std::thread::sleep(Duration::from_millis(150));
  registry.kill("kill", Some("SIGKILL")).expect("kill");

  let (exit_code, signal) = wait_for_exit(&sink, "kill");
  assert_eq!(signal, Some(libc::SIGKILL));
  assert_eq!(exit_code, None);
  assert!(!sink.data_text("kill").contains("never"));
  registry.forget("kill");
}

#[test]
fn unknown_signal_names_are_rejected() {
  let (registry, _sink) = registry();
  registry
    .create_or_attach(&params("badsignal", &[], 80, 24))
    .expect("create");
  let error = registry.kill("badsignal", Some("SIGBREAKFAST")).expect_err("rejected");
  assert!(matches!(error, wand_render::RenderError::BadRequest(_)));
  registry.forget("badsignal");
}

#[test]
fn forget_drops_the_record_and_stops_journaling() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params("forget", &[], 80, 24))
    .expect("create");
  registry.write("forget", "echo before-forget\r").expect("write");
  assert!(wait_until(WAIT, || sink
    .data_text("forget")
    .contains("before-forget")));
  registry.forget("forget");
  assert!(registry.attach("forget", 0).is_none());
  assert_eq!(registry.session_count(), 0);

  let recreated = registry
    .create_or_attach(&params("forget", &[], 80, 24))
    .expect("create again");
  assert!(recreated.is_new);
  registry.forget("forget");
}

#[test]
fn spawn_rejects_bad_requests() {
  let (registry, _sink) = registry();
  let mut missing_cwd = params("bad", &["-c", "true"], 80, 24);
  missing_cwd.cwd = "/definitely/not/here".into();
  assert!(matches!(
    registry.create_or_attach(&missing_cwd),
    Err(wand_render::RenderError::BadRequest(_))
  ));

  let mut zero_size = params("bad-size", &["-c", "true"], 0, 24);
  zero_size.cols = 0;
  assert!(matches!(
    registry.create_or_attach(&zero_size),
    Err(wand_render::RenderError::BadRequest(_))
  ));

  let mut missing_file = params("bad-file", &[], 80, 24);
  missing_file.file = "/definitely/not/here/sh".into();
  assert!(matches!(
    registry.create_or_attach(&missing_file),
    Err(wand_render::RenderError::Spawn(_))
  ));
}

/// 屏幕快照在真实 PTY 输出后依然可用，并能用新会话等价重建屏幕。
#[test]
fn terminal_snapshot_replays_real_pty_output() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params(
      "snapshot",
      &["-c", "printf '\\033[31mred\\033[0m\\r\\n'; sleep 0.3"],
      40,
      10,
    ))
    .expect("create");
  assert!(wait_until(WAIT, || sink.data_text("snapshot").contains("red")));
  // 等静默窗口把基线固化。
  assert!(wait_until(WAIT, || {
    registry
      .attach("snapshot", 0)
      .and_then(|state| state.terminal_snapshot)
      .is_some_and(|snapshot| !snapshot.pending.is_empty() || snapshot.data.contains("red"))
  }));

  let state = registry.attach("snapshot", 0).expect("attach");
  let snapshot = state.terminal_snapshot.expect("snapshot");
  assert_eq!(snapshot.version, 1);
  assert_eq!((snapshot.cols, snapshot.rows), (40, 10));

  // 把快照喂进一个全新的同尺寸 VT 解析器，屏幕内容必须与 Render 的模型一致。
  let mut replayed = vt100::Parser::new(snapshot.rows, snapshot.cols, 0);
  replayed.process(snapshot.data.as_bytes());
  for op in &snapshot.pending {
    match op {
      wand_render_protocol::PendingOp::Data { data } => replayed.process(data.as_bytes()),
      wand_render_protocol::PendingOp::Resize { cols, rows } => {
        replayed.screen_mut().set_size(*rows, *cols);
      }
    }
  }
  assert!(replayed.screen().contents().contains("red"));
  registry.forget("snapshot");
}

#[test]
fn snapshot_baseline_never_double_applies_pending() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params(
      "consistency",
      &["-c", "printf baseline-text; sleep 0.6"],
      40,
      10,
    ))
    .expect("create");
  assert!(wait_until(WAIT, || sink.data_text("consistency").contains("baseline-text")));
  // 等静默窗口结束：基线必须已包含全部输出，pending 必须为空。
  assert!(wait_until(WAIT, || {
    registry
      .attach("consistency", 0)
      .and_then(|state| state.terminal_snapshot)
      .is_some_and(|snapshot| snapshot.data.contains("baseline-text"))
  }));
  let state = registry.attach("consistency", 0).expect("attach");
  let snapshot = state.terminal_snapshot.clone().expect("snapshot");
  assert!(
    snapshot.pending.is_empty(),
    "a fresh baseline must not carry pending ops: {:?}",
    snapshot.pending
  );

  // 基线 + pending 重放出来的屏幕，必须与从头重放 output 的结果一致。
  let replay = |data: &str, ops: &[wand_render_protocol::PendingOp]| {
    let mut parser = vt100::Parser::new(snapshot.rows, snapshot.cols, 0);
    parser.process(data.as_bytes());
    for op in ops {
      match op {
        wand_render_protocol::PendingOp::Data { data } => parser.process(data.as_bytes()),
        wand_render_protocol::PendingOp::Resize { cols, rows } => {
          parser.screen_mut().set_size(*rows, *cols);
        }
      }
    }
    parser.screen().contents()
  };
  assert_eq!(
    replay(&snapshot.data, &snapshot.pending),
    replay(&state.output, &[]),
    "snapshot data + pending must be screen-equivalent to the journal"
  );
  registry.forget("consistency");
}

#[test]
fn snapshot_never_loses_output_between_checkpoints() {
  // 「baseline + pending == 当前屏幕」必须在任何时刻成立：读到快照后立刻重放，
  // 屏幕内容必须与从头重放 journal 完全一致（少一段就是重连丢输出）。
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params(
      "midflight",
      &["-c", "printf ALPHA; sleep 0.05; printf BETA; sleep 5"],
      40,
      10,
    ))
    .expect("create");
  assert!(wait_until(WAIT, || sink.data_text("midflight").contains("BETA")));

  let state = registry.attach("midflight", 0).expect("attach");
  let snapshot = state.terminal_snapshot.clone().expect("snapshot");
  let pending_data: String = snapshot
    .pending
    .iter()
    .filter_map(|op| match op {
      wand_render_protocol::PendingOp::Data { data } => Some(data.as_str()),
      _ => None,
    })
    .collect();
  assert!(
    snapshot.data.contains("BETA") || pending_data.contains("BETA"),
    "BETA is in neither the baseline nor pending: data={:?} pending={:?}",
    snapshot.data,
    snapshot.pending
  );
  assert!(
    !(snapshot.data.contains("BETA") && pending_data.contains("BETA")),
    "BETA was applied twice (baseline and pending)"
  );

  let replay = |data: &str, ops: &[wand_render_protocol::PendingOp]| {
    let mut parser = vt100::Parser::new(snapshot.rows, snapshot.cols, 0);
    parser.process(data.as_bytes());
    for op in ops {
      match op {
        wand_render_protocol::PendingOp::Data { data } => parser.process(data.as_bytes()),
        wand_render_protocol::PendingOp::Resize { cols, rows } => {
          parser.screen_mut().set_size(*rows, *cols);
        }
      }
    }
    parser.screen().contents()
  };
  assert_eq!(
    replay(&snapshot.data, &snapshot.pending),
    replay(&state.output, &[]),
    "baseline + pending must be screen-equivalent to the journal"
  );
  registry.forget("midflight");
}

#[test]
fn resize_is_reflected_as_a_pending_operation_until_the_next_checkpoint() {
  let (registry, _sink) = registry();
  registry
    .create_or_attach(&params("resize-op", &["-c", "sleep 5"], 80, 24))
    .expect("create");
  registry.resize("resize-op", 100, 40).expect("resize");
  let state = registry.attach("resize-op", 0).expect("attach");
  assert_eq!((state.cols, state.rows), (100, 40));
  let snapshot = state.terminal_snapshot.expect("snapshot");
  // 基线还没重拍（静默窗口未到）时，尺寸变化必须出现在 pending 里，
  // 否则客户端只能拿到旧尺寸的屏幕。
  if (snapshot.cols, snapshot.rows) != (100, 40) {
    assert_eq!((snapshot.cols, snapshot.rows), (80, 24));
    assert!(
      snapshot.pending.iter().any(|op| matches!(
        op,
        wand_render_protocol::PendingOp::Resize { cols: 100, rows: 40 }
      )),
      "resize is missing from pending: {:?}",
      snapshot.pending
    );
  }
  registry.forget("resize-op");
}

#[test]
fn awk_is_available_for_these_tests() {
  // 显式声明依赖：上面几个测试用 awk 生成可预期的输出量。
  assert!(std::path::Path::new(AWK).exists(), "{AWK} is required");
}

/// §9.1.1：`list` 的快照必须有界（序列化后 ≤ 64KiB），而 `attach` 仍返回完整状态。
///
/// 这里用「200 列 × 6000 行」造出真实的大回滚屏幕：journal 只保留尾 20 万字符，
/// 但 VT 屏幕模型会留下 5000 行历史，快照因此远大于 64KiB —— 正是 `list` 撞上
/// 单帧上限的那个形态。
#[test]
fn list_bounds_the_snapshot_while_attach_stays_complete() {
  let (registry, sink) = registry();
  let script = "awk 'BEGIN{for(i=0;i<6000;i++){for(j=0;j<200;j++)printf \"x\"; printf \"\\n\"}}'";
  registry
    .create_or_attach(&params("wide", &["-c", script], 200, 60))
    .expect("create");
  wait_for_exit(&sink, "wide");

  let listed = registry.list_sessions();
  assert_eq!(listed.len(), 1);
  let listed = &listed[0];
  let listed_snapshot = listed
    .terminal_snapshot
    .as_ref()
    .expect("list keeps a bounded snapshot hint");
  let listed_bytes = serde_json::to_vec(listed_snapshot).expect("json").len();
  assert!(
    listed_bytes <= LIST_SNAPSHOT_MAX_BYTES,
    "list snapshot grew to {listed_bytes} bytes"
  );

  let full = registry.attach("wide", 0).expect("attach");
  let full_snapshot = full.terminal_snapshot.as_ref().expect("snapshot");
  let full_bytes = serde_json::to_vec(full_snapshot).expect("json").len();
  assert!(
    full_bytes > LIST_SNAPSHOT_MAX_BYTES,
    "attach must return the unbounded snapshot, got only {full_bytes} bytes"
  );
  // output / chunks 不受快照裁剪影响（§9.1.1：仍按 §4 的上限）。
  assert!(listed.output.chars().count() <= PTY_OUTPUT_MAX_CHARS);
  assert_eq!(listed.output, full.output);
  assert_eq!(listed.chunks.len(), full.chunks.len());
  assert_eq!(listed.status, full.status);
  registry.forget("wide");
}

/// §9.4：不传 signal 时投递 SIGHUP（「关闭终端」语义），不是 SIGTERM。
#[test]
fn kill_without_a_signal_uses_sighup() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params("hup", &["-c", "sleep 30"], 80, 24))
    .expect("create");
  std::thread::sleep(Duration::from_millis(150));
  registry.kill("hup", None).expect("kill");
  let (exit_code, signal) = wait_for_exit(&sink, "hup");
  assert_eq!(
    signal,
    Some(libc::SIGHUP),
    "the default kill signal must be SIGHUP (protocol §9.4)"
  );
  assert_eq!(exit_code, None);
  registry.forget("hup");
}

/// §9.3：drain 不杀运行中的 PTY、不接受新会话；`now` 才杀掉全部。
#[test]
fn drain_keeps_running_sessions_and_refuses_new_ones() {
  let (registry, sink) = registry();
  registry
    .create_or_attach(&params("keep", &["-c", "sleep 30"], 80, 24))
    .expect("create");
  registry.begin_shutdown(ShutdownMode::Drain);
  assert!(registry.is_shutting_down());
  assert_eq!(registry.running_session_count(), 1);
  let state = registry.attach("keep", 0).expect("attach");
  assert_eq!(state.status, SessionStatus::Running);
  assert!(matches!(
    registry.create_or_attach(&params("new", &["-c", "true"], 80, 24)),
    Err(RenderError::Conflict(_))
  ));

  // now：杀掉所有 PTY。
  registry.begin_shutdown(ShutdownMode::Now);
  assert_eq!(registry.session_count(), 0);
  assert_eq!(registry.running_session_count(), 0);
  let (_, signal) = wait_for_exit(&sink, "keep");
  assert_eq!(signal, Some(libc::SIGKILL));
  registry.stop_maintenance();
}
