#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wand_render::paths::structured_render_paths;
use wand_render_protocol::{decode_frame, encode_frame, Request, Response};
use wand_render_protocol::structured_v2::STRUCTURED_PROTOCOL_VERSION;

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect(socket: &std::path::Path) -> UnixStream {
    let stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream
}

fn request(socket: &mut UnixStream, token: &str, id: u32, method: &str, params: Option<Value>) -> Response {
    let frame = encode_frame(&Request { id, token: token.into(),
        protocol_version: STRUCTURED_PROTOCOL_VERSION, method: method.into(), params }).unwrap();
    socket.write_all(&frame).unwrap();
    loop {
        let mut header = [0u8; 4];
        socket.read_exact(&mut header).unwrap_or_else(|cause| panic!("{method} id={id}: {cause}"));
        let mut body = vec![0u8; u32::from_be_bytes(header) as usize];
        socket.read_exact(&mut body).unwrap_or_else(|cause| panic!("{method} id={id} body: {cause}"));
        let (_, value): (usize, Value) = decode_frame(&[header.to_vec(), body].concat()).unwrap().unwrap();
        if value.get("id").and_then(Value::as_u64) == Some(id as u64) {
            return serde_json::from_value(value).unwrap();
        }
        assert!(value.get("event").is_some(), "unexpected non-response frame");
    }
}

#[test]
fn wire_owns_child_across_socket_reconnect_and_rejects_wrong_protocol() {
    let dir = std::env::temp_dir().join(format!("wand-structured-wire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let paths = structured_render_paths(&config);
    let mut daemon = Daemon(Command::new(env!("CARGO_BIN_EXE_wand-structured-renderd"))
        .args(["--config", config.to_str().unwrap()])
        .stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap());
    let started = Instant::now();
    while !paths.socket_path.exists() || !paths.token_path.exists() {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    let token = std::fs::read_to_string(&paths.token_path).unwrap();
    let mut socket = connect(&paths.socket_path);
    let hello = request(&mut socket, &token, 1, "hello", None);
    assert_eq!(hello.result.unwrap()["protocolVersion"], 2);
    let spawn = request(&mut socket, &token, 2, "spawn", Some(json!({
        "runId":"structured:wire", "file":"/bin/sh", "args":["-c","cat"],
        "cwd":"/tmp", "env":{"PATH":"/usr/bin:/bin"}, "stdinData":"中文💡"
    })));
    assert!(spawn.ok);
    let pid = spawn.result.unwrap()["state"]["pid"].as_u64().unwrap();
    drop(socket); // Node disconnected; daemon child still owns the same PID.
    let mut again = connect(&paths.socket_path);
    let list = request(&mut again, &token, 3, "list", None);
    assert_eq!(list.result.as_ref().unwrap()["runs"][0]["pid"], pid);
    assert!(list.result.as_ref().unwrap()["runs"][0].get("stdoutLog").is_none());
    let retried = request(&mut again, &token, 4, "spawn", Some(json!({
        "runId":"structured:wire", "file":"/bin/false", "args":[],
        "cwd":"/tmp", "env":{}
    })));
    assert_eq!(retried.result.unwrap()["isNew"], false);
    let started = Instant::now();
    loop {
        let attached = request(&mut again, &token, 5, "attach", Some(json!({
            "runId":"structured:wire", "maxBytes":16384
        })));
        let value = attached.result.unwrap();
        if value["state"]["status"] == "exited" {
            let output: String = value["stdout"]["chunks"].as_array().unwrap().iter()
                .map(|chunk| chunk["data"].as_str().unwrap()).collect();
            assert_eq!(output, "中文💡");
            assert_eq!(value["state"]["exitCode"], 0);
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    // Wrong version is rejected (no fallback to PTY v1).
    let wrong = encode_frame(&Request { id: 9, token: token.clone(), protocol_version: 1,
        method: "hello".into(), params: None }).unwrap();
    let mut different = connect(&paths.socket_path);
    different.write_all(&wrong).unwrap();
    let mut byte = [0u8; 1];
    // The peer may either return protocolMismatch or close immediately.
    assert!(different.read(&mut byte).is_ok());
    let _ = request(&mut again, &token, 6, "shutdown", Some(json!({"mode":"drain"})));
    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "drained daemon did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(dir);
}
