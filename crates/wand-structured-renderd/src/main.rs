//! Independent structured process daemon. PTY Render v1 stays untouched.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use wand_render::paths::structured_render_paths;
use wand_render::security::{self, ExistingSocket};
use wand_render_protocol::{encode_frame, ErrorBody, ErrorCode, Request, Response, MAX_FRAME_BYTES};
use wand_render_protocol::structured_v2::{
    AttachParams, HelloResult, InterruptParams, RunIdParams, SpawnParams, StatsResult,
    STRUCTURED_PROTOCOL_VERSION,
};
use wand_structured_renderd::Registry;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SPAWN_FRAME_BYTES: usize = 16 * 1024 * 1024;
const EVENT_QUEUE_DEPTH: usize = 256;
static SIGNAL_COUNT: AtomicU8 = AtomicU8::new(0);

extern "C" fn shutdown_signal(_: libc::c_int) {
    SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
}

fn main() {
    if let Err(error) = run() {
        eprintln!("wand-structured-render: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("--version"), None) => {
            println!("wand-structured-render {VERSION} (protocol {STRUCTURED_PROTOCOL_VERSION})");
            Ok(())
        }
        (Some("--config"), Some(config)) if args.next().is_none() => serve(Path::new(&config)),
        _ => Err(anyhow!("usage: wand-structured-render --config <config-path> | --version")),
    }
}

struct Connection {
    writer: SyncSender<Vec<u8>>,
    stream: UnixStream,
    authed: AtomicBool,
}

impl Connection {
    fn close(&self) { let _ = self.stream.shutdown(std::net::Shutdown::Both); }
    fn send(&self, value: &impl serde::Serialize) -> bool {
        encode_frame(value).ok().is_some_and(|frame| self.writer.send(frame).is_ok())
    }
}

#[derive(Default)]
struct Hub {
    clients: Mutex<Vec<Arc<Connection>>>,
}

impl Hub {
    fn publish(&self, event: wand_render_protocol::structured_v2::Event) {
        if let Ok(frame) = encode_frame(&event) {
            let mut clients = self.clients.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            clients.retain(|client| {
                if !client.authed.load(Ordering::Acquire) { return true; }
                match client.writer.try_send(frame.clone()) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                        client.close(); false
                    }
                }
            });
        }
    }
}

fn token_match(expected: &str, actual: &str) -> bool {
    let a = expected.as_bytes();
    let b = actual.as_bytes();
    let mut diff = a.len() ^ b.len(); // usize, never truncate length modulo 256
    for index in 0..a.len().max(b.len()) {
        diff |= (a.get(index).copied().unwrap_or_default()
            ^ b.get(index).copied().unwrap_or_default()) as usize;
    }
    diff == 0
}

fn read_frame(stream: &mut UnixStream) -> std::io::Result<(Value, usize)> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header);
    if len > MAX_FRAME_BYTES || len as usize > MAX_SPAWN_FRAME_BYTES {
        return Err(std::io::Error::other("oversized request frame"));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body)?;
    let value = serde_json::from_slice(&body)
        .map_err(|_| std::io::Error::other("invalid frame"))?;
    Ok((value, len as usize))
}

fn error(id: u32, code: ErrorCode, message: &str) -> Response {
    Response { id, ok: false, result: None, error: Some(ErrorBody { code, message: message.into() }) }
}

fn ok(id: u32, value: impl serde::Serialize) -> Response {
    Response { id, ok: true, result: serde_json::to_value(value).ok(), error: None }
}

fn parse_params<T: DeserializeOwned>(request: &Request) -> Result<T, Response> {
    serde_json::from_value(request.params.clone().unwrap_or(Value::Null))
        .map_err(|_| error(request.id, ErrorCode::BadRequest, "invalid structured request parameters"))
}

fn dispatch(registry: &Arc<Registry>, shutdown: &AtomicU8, started_at: &str,
            request: &Request, size: usize) -> Response {
    let id = request.id;
    match request.method.as_str() {
        "hello" => ok(id, HelloResult {
            version: VERSION.into(), protocol_version: STRUCTURED_PROTOCOL_VERSION,
            pid: std::process::id(), started_at: started_at.into(), runs: registry.list().runs.len(),
        }),
        "ping" => ok(id, json!({ "pong": true })),
        "list" => ok(id, registry.list()),
        "spawn" => {
            if size > MAX_SPAWN_FRAME_BYTES {
                return error(id, ErrorCode::BadRequest, "structured spawn frame exceeds limit");
            }
            let params = match parse_params::<SpawnParams>(request) { Ok(params) => params, Err(e) => return e };
            match registry.spawn(params) {
                Ok(value) => ok(id, value),
                Err(message) => error(id, ErrorCode::Conflict, &message),
            }
        }
        "attach" => {
            let params = match parse_params::<AttachParams>(request) { Ok(params) => params, Err(e) => return e };
            match registry.attach(params) {
                Some(value) => ok(id, value),
                None => error(id, ErrorCode::NotFound, "unknown structured run"),
            }
        }
        "interrupt" => {
            let params = match parse_params::<InterruptParams>(request) { Ok(params) => params, Err(e) => return e };
            match registry.interrupt(&params.run_id, params.signal.as_deref().unwrap_or("SIGTERM")) {
                Ok(()) => ok(id, json!({})),
                Err(message) => error(id, ErrorCode::BadRequest, &message),
            }
        }
        "forget" => {
            let params = match parse_params::<RunIdParams>(request) { Ok(params) => params, Err(e) => return e };
            match registry.forget(&params.run_id) {
                Ok(()) => ok(id, json!({})),
                Err(message) => error(id, ErrorCode::NotFound, &message),
            }
        }
        "stats" => {
            let runs = registry.list().runs.len();
            ok(id, StatsResult { runs, running_runs: registry.running_count(),
                retained_log_bytes: registry.retained_bytes(),
                rss_bytes: wand_render::resources::rss_bytes() })
        }
        "shutdown" => {
            let mode = request.params.as_ref().and_then(|p| p.get("mode"))
                .and_then(Value::as_str);
            match mode {
                Some("drain") => { registry.begin_drain(); shutdown.store(1, Ordering::Release); ok(id, json!({})) }
                Some("now") => { registry.stop_now(); shutdown.store(2, Ordering::Release); ok(id, json!({})) }
                _ => error(id, ErrorCode::BadRequest, "expected shutdown mode drain or now"),
            }
        }
        _ => error(id, ErrorCode::UnsupportedMethod, "unsupported structured method"),
    }
}

fn handle_connection(mut stream: UnixStream, token: String, hub: Arc<Hub>,
                     registry: Arc<Registry>, shutdown: Arc<AtomicU8>, started_at: String) {
    if security::ensure_peer_uid(&stream, security::current_uid()).is_err() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    }
    if stream.set_nonblocking(false).is_err() { return; }
    let writer_stream = match stream.try_clone() { Ok(clone) => clone, Err(_) => return };
    let close_stream = match stream.try_clone() { Ok(clone) => clone, Err(_) => return };
    let (tx, rx) = sync_channel::<Vec<u8>>(EVENT_QUEUE_DEPTH);
    let conn = Arc::new(Connection { writer: tx, stream: close_stream, authed: AtomicBool::new(false) });
    hub.clients.lock().unwrap().push(Arc::clone(&conn));
    let handle = std::thread::spawn(move || {
        let mut writer = writer_stream;
        while let Ok(frame) = rx.recv() {
            if writer.write_all(&frame).is_err() { break; }
        }
    });
    // A silent unauthenticated client cannot pin a connection indefinitely.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    loop {
        let (value, size) = match read_frame(&mut stream) {
            Ok(value) => value,
            Err(_) => break,
        };
        let request: Request = match serde_json::from_value(value) { Ok(request) => request, Err(_) => break };
        if !token_match(&token, &request.token) { break; }
        if request.protocol_version != STRUCTURED_PROTOCOL_VERSION {
            let _ = conn.send(&error(request.id, ErrorCode::ProtocolMismatch, "structured protocol version mismatch"));
            break;
        }
        conn.authed.store(true, Ordering::Release);
        let _ = stream.set_read_timeout(None);
        if !conn.send(&dispatch(&registry, &shutdown, &started_at, &request, size)) { break; }
    }
    conn.authed.store(false, Ordering::Release);
    conn.close();
    hub.clients.lock().unwrap().retain(|client| !Arc::ptr_eq(client, &conn));
    drop(conn);
    // A blocked writer on a dead peer has had its socket shut down above.
    let _ = handle.join();
}

fn write_private(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::fs::OpenOptions;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn serve(config: &Path) -> Result<()> {
    let paths = structured_render_paths(config);
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, shutdown_signal as *const () as usize);
        libc::signal(libc::SIGINT, shutdown_signal as *const () as usize);
        libc::umask(0o077);
    }
    std::fs::create_dir_all(paths.config_dir())?;
    match security::inspect_socket_path(&paths.socket_path) {
        ExistingSocket::Absent => {
            // A missing pathname does not imply a dead owner. Let its recovery
            // loop rebind; never rotate credentials underneath live runs.
            if let Ok(pid) = std::fs::read_to_string(&paths.pid_path) {
                if let Ok(pid) = pid.trim().parse::<i32>() {
                    if pid > 0 && (unsafe { libc::kill(pid, 0) } == 0
                        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)) {
                        return Err(anyhow!("structured Render owner is alive; waiting for socket recovery"));
                    }
                }
            }
        },
        ExistingSocket::Owned => {
            if UnixStream::connect(&paths.socket_path).is_ok() {
                return Err(anyhow!("another structured Render daemon is listening"));
            }
            std::fs::remove_file(&paths.socket_path)?;
        }
        ExistingSocket::Foreign(_) => return Err(anyhow!("refusing foreign structured socket")),
    }
    let listener = UnixListener::bind(&paths.socket_path).context("bind structured socket")?;
    std::fs::set_permissions(&paths.socket_path, std::fs::Permissions::from_mode(0o600))?;
    let mut listener = wand_render::socket_keepalive::RecoveringListener::new(
        listener, paths.socket_path.clone())?;
    let token = random_token()?;
    write_private(&paths.token_path, token.as_bytes(), 0o600)?;
    write_private(&paths.pid_path, format!("{}\n", std::process::id()).as_bytes(), 0o644)?;
    let started_at = wand_render::time::iso8601_now();
    write_private(&paths.meta_path, json!({"version":VERSION,"protocolVersion":2,
        "pid":std::process::id(),"startedAt":started_at}).to_string().as_bytes(), 0o644)?;
    let hub = Arc::new(Hub::default());
    let publisher = Arc::clone(&hub);
    let registry = Registry::new(Arc::new(move |event| publisher.publish(event)));
    let shutdown = Arc::new(AtomicU8::new(0));
    loop {
        let signals = SIGNAL_COUNT.swap(0, Ordering::AcqRel);
        if signals > 0 {
            if signals >= 2 || shutdown.load(Ordering::Acquire) > 0 {
                registry.stop_now(); shutdown.store(2, Ordering::Release);
            } else { registry.begin_drain(); shutdown.store(1, Ordering::Release); }
        }
        if shutdown.load(Ordering::Acquire) == 2 ||
            (shutdown.load(Ordering::Acquire) == 1 && registry.running_count() == 0) { break; }
        match listener.accept() {
            Ok((stream, _)) => {
                let hub = Arc::clone(&hub);
                let registry = Arc::clone(&registry);
                let shutdown = Arc::clone(&shutdown);
                let token = token.clone();
                let started_at = started_at.clone();
                std::thread::spawn(move || handle_connection(stream, token, hub, registry, shutdown, started_at));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    // Give shutdown responses time to leave before closing the socket.
    std::thread::sleep(Duration::from_millis(150));
    for path in [&paths.socket_path, &paths.token_path, &paths.pid_path, &paths.meta_path] {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_length_cannot_wrap_modulo_256() {
        let token = "a".repeat(64);
        assert!(token_match(&token, &token));
        assert!(!token_match(&token, &(token.clone() + &"\0".repeat(256))));
    }
}
