#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wand_render_protocol::structured_v2::{AttachParams, Event, RunStatus, SpawnParams};
use wand_structured_renderd::Registry;

fn request(run_id: &str, script: &str, stdin_data: Option<&str>) -> SpawnParams {
    SpawnParams {
        run_id: run_id.into(), file: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()], cwd: "/tmp".into(),
        env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        stdin_data: stdin_data.map(str::to_owned),
    }
}

fn until_exited(registry: &Registry, run_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state = registry.list().runs.into_iter().find(|run| run.run_id == run_id).unwrap();
        if state.status == RunStatus::Exited { return; }
        assert!(Instant::now() < deadline, "process did not exit");
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn attach(registry: &Registry, run_id: &str, stdout: u64, stderr: u64) -> wand_render_protocol::structured_v2::AttachResult {
    registry.attach(AttachParams {
        run_id: run_id.into(), after_stdout_seq: stdout,
        after_stderr_seq: stderr, max_bytes: None,
    }).expect("known run")
}

#[test]
fn stdin_once_two_streams_and_repeated_spawn_are_owned_by_one_child() {
    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let registry = Registry::new(Arc::new(move |event| sink.lock().unwrap().push(event)));
    let params = request("structured:test", "cat; printf 'ERR' >&2", Some("中文💡\n"));
    let first = registry.spawn(params.clone()).unwrap();
    assert!(first.is_new);
    let second = registry.spawn(params.clone()).unwrap();
    assert!(!second.is_new);
    assert_eq!(first.state.pid, second.state.pid);
    until_exited(&registry, &params.run_id);
    let third = registry.spawn(params).unwrap();
    assert!(!third.is_new);
    assert_eq!(first.state.incarnation_id, third.state.incarnation_id);
    let replay = attach(&registry, "structured:test", 0, 0);
    assert_eq!(replay.stdout.chunks.iter().map(|chunk| chunk.data.as_str()).collect::<String>(), "中文💡\n");
    assert_eq!(replay.stderr.chunks.iter().map(|chunk| chunk.data.as_str()).collect::<String>(), "ERR");
    assert_eq!(replay.state.exit_code, Some(0));
    assert!(replay.stdout.complete && replay.stderr.complete);
    assert!(!replay.stdout.reset_required && !replay.stderr.reset_required);
    let observed = events.lock().unwrap();
    assert_eq!(observed.iter().filter(|event| matches!(event, Event::Exit { .. })).count(), 1);
    assert!(matches!(observed.last(), Some(Event::Exit { .. })));
}

#[test]
fn early_stdin_close_is_not_reported_as_a_successful_turn() {
    let registry = Registry::new(Arc::new(|_| {}));
    let input = "x".repeat(2 * 1024 * 1024);
    registry.spawn(request("structured:closed-stdin", "exit 0", Some(&input))).unwrap();
    until_exited(&registry, "structured:closed-stdin");
    assert_eq!(attach(&registry, "structured:closed-stdin", 0, 0).state.exit_code, Some(-1));
}

#[test]
fn pagination_and_drain_keep_live_process_and_detect_expired_cursors() {
    let registry = Registry::new(Arc::new(|_| {}));
    let run = registry.spawn(request("structured:pages", "yes x | head -c 8500000", None)).unwrap();
    until_exited(&registry, "structured:pages");
    let mut cursor = 0;
    let mut pages = 0;
    loop {
        let page = registry.attach(AttachParams { run_id: run.state.run_id.clone(),
            after_stdout_seq: cursor, after_stderr_seq: 0, max_bytes: Some(8192) }).unwrap();
        assert!(page.state.stdout_truncated);
        if pages == 0 { assert!(page.stdout.reset_required); }
        assert!(page.stdout.next_seq > cursor);
        cursor = page.stdout.next_seq;
        pages += 1;
        if page.stdout.complete { break; }
        assert!(pages < 2000);
    }
    assert!(pages > 1);
    assert!(registry.retained_bytes() <= 8 * 1024 * 1024);
    registry.begin_drain();
    assert!(registry.spawn(request("structured:new", "echo no", None)).is_err());
    assert_eq!(registry.list().runs.len(), 1);
    registry.forget("structured:pages").unwrap();
    assert!(registry.list().runs.is_empty());
}

#[test]
fn explicit_interrupt_reports_signal_without_killing_other_runs() {
    let registry = Registry::new(Arc::new(|_| {}));
    registry.spawn(request("structured:slow", "exec sleep 30", None)).unwrap();
    registry.spawn(request("structured:fast", "printf ok", None)).unwrap();
    registry.interrupt("structured:slow", "SIGTERM").unwrap();
    until_exited(&registry, "structured:slow");
    until_exited(&registry, "structured:fast");
    assert_eq!(attach(&registry, "structured:slow", 0, 0).state.signal, Some(libc::SIGTERM));
    assert_eq!(attach(&registry, "structured:fast", 0, 0).state.exit_code, Some(0));
    assert!(registry.interrupt("structured:fast", "UNKNOWN").is_err());
}
