//! v2 structured CLI owner. No PTYs, HTTP, database or provider projections here.
//! Each child, both pipes, and its bounded replay window survive Node disconnects.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use wand_render::utf8::IncrementalUtf8Decoder;
use wand_render_protocol::structured_v2::{
    AttachParams, AttachResult, Event, ListResult, ReplayStream, RunState, RunStatus,
    SpawnParams, SpawnResult, StreamChunk, StreamName, REPLAY_PAGE_MAX_BYTES, RUN_LOG_MAX_BYTES,
};

// 8 runs × 2 streams × 8 MiB: absolute replay capacity <= 128 MiB.
// Count retained exited runs too, until forget explicitly releases the slot.
const MAX_ADMITTED_RUNS: usize = 8;
fn incarnation_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| "failed to obtain structured incarnation entropy".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Default)]
struct Log {
    chunks: VecDeque<StreamChunk>,
    size: usize,
    seq: u64,
    truncated: bool,
}

impl Log {
    fn append(&mut self, data: String) -> u64 {
        self.seq += 1;
        self.size += data.len();
        self.chunks.push_back(StreamChunk { seq: self.seq, data });
        while self.size > RUN_LOG_MAX_BYTES {
            if let Some(old) = self.chunks.pop_front() {
                self.size -= old.data.len();
                self.truncated = true;
            } else { break; }
        }
        self.seq
    }

    fn page(&self, after: u64, remaining: &mut usize) -> ReplayStream {
        let first = self.chunks.front().map_or(self.seq + 1, |chunk| chunk.seq);
        let reset_required = after > self.seq || after < first.saturating_sub(1);
        let cursor = if reset_required { first.saturating_sub(1) } else { after };
        let mut chunks = Vec::new();
        let mut next_seq = cursor;
        for chunk in self.chunks.iter().filter(|chunk| chunk.seq > cursor) {
            // Reader emits at most 8 KiB per decoded chunk; allow one chunk on
            // an otherwise empty page even if the caller asked for a tiny page.
            if chunk.data.len() > *remaining { break; }
            *remaining = remaining.saturating_sub(chunk.data.len());
            next_seq = chunk.seq;
            chunks.push(chunk.clone());
        }
        ReplayStream { chunks, next_seq, complete: next_seq == self.seq, reset_required }
    }
}

struct RunInner {
    status: RunStatus,
    exit_code: Option<i32>,
    signal: Option<i32>,
    stdout: Log,
    stderr: Log,
}

struct Run {
    id: String,
    incarnation: String,
    pid: u32,
    inner: Mutex<RunInner>,
}

impl Run {
    fn state(&self, inner: &RunInner) -> RunState {
        RunState {
            run_id: self.id.clone(), incarnation_id: self.incarnation.clone(), pid: self.pid,
            status: inner.status, exit_code: inner.exit_code, signal: inner.signal,
            stdout_seq: inner.stdout.seq, stderr_seq: inner.stderr.seq,
            stdout_truncated: inner.stdout.truncated, stderr_truncated: inner.stderr.truncated,
        }
    }

    fn snapshot(&self) -> RunState {
        self.state(&self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

pub struct Registry {
    runs: Mutex<HashMap<String, Arc<Run>>>,
    draining: AtomicBool,
    publish: Arc<dyn Fn(Event) + Send + Sync>,
}

impl Registry {
    pub fn new(publish: Arc<dyn Fn(Event) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { runs: Mutex::new(HashMap::new()), draining: AtomicBool::new(false), publish })
    }

    /// Reservation and Command::spawn occur under the same lock: competing
    /// callers never launch a second process for the same runId, including
    /// when the first process exits before its readers can attach.
    pub fn spawn(self: &Arc<Self>, params: SpawnParams) -> Result<SpawnResult, String> {
        if params.run_id.is_empty() || params.file.is_empty() || params.cwd.is_empty() {
            return Err("missing required structured process field".into());
        }
        let mut runs = self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = runs.get(&params.run_id) {
            return Ok(SpawnResult { state: existing.snapshot(), is_new: false });
        }
        if self.draining.load(Ordering::SeqCst) {
            return Err("structured daemon is draining".into());
        }
        if runs.len() >= MAX_ADMITTED_RUNS {
            return Err("structured process capacity exhausted; existing runs were not killed".into());
        }
        let incarnation = incarnation_id()?;
        let mut command = Command::new(&params.file);
        command.args(&params.args).current_dir(&params.cwd).env_clear().envs(&params.env)
            .stdin(if params.stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                // Match the StructuredExecHost seam: an exec failure is an
                // exited run (pid 0, negative OS code), still attachable and
                // never respawned on a duplicate request.
                let exit_code = error.raw_os_error().map(|code| -code).or(Some(-1));
                let run = Arc::new(Run {
                    id: params.run_id.clone(), incarnation,
                    pid: 0,
                    inner: Mutex::new(RunInner { status: RunStatus::Exited, exit_code,
                        signal: None, stdout: Log::default(), stderr: Log::default() }),
                });
                let state = run.snapshot();
                runs.insert(params.run_id.clone(), Arc::clone(&run));
                drop(runs);
                (self.publish)(Event::Exit { run_id: params.run_id,
                    incarnation_id: run.incarnation.clone(), exit_code, signal: None });
                return Ok(SpawnResult { state, is_new: true });
            }
        };
        let pid = child.id();
        let run = Arc::new(Run {
            id: params.run_id.clone(), incarnation, pid,
            inner: Mutex::new(RunInner { status: RunStatus::Running, exit_code: None,
                signal: None, stdout: Log::default(), stderr: Log::default() }),
        });
        // Put the record in the inventory before readers or the reaper can
        // publish any events. The inventory is the recovery authority.
        runs.insert(params.run_id, Arc::clone(&run));
        let state = run.snapshot();
        drop(runs);

        let input_writer = params.stdin_data.map(|data| {
            let stdin = child.stdin.take();
            std::thread::spawn(move || {
                // A CLI can exit before reading its prompt. EPIPE must not be
                // reported as a successful turn just because the child exits 0.
                stdin.map_or(true, |mut pipe| pipe.write_all(data.as_bytes()).is_err())
            })
        });
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let out_reader = read_pipe(stdout, Arc::clone(&run), StreamName::Stdout, Arc::clone(&self.publish));
        let err_reader = read_pipe(stderr, Arc::clone(&run), StreamName::Stderr, Arc::clone(&self.publish));
        let publisher = Arc::clone(&self.publish);
        std::thread::spawn(move || {
            let result = child.wait();
            let _ = out_reader.join();
            let _ = err_reader.join();
            let stdin_failed = input_writer.is_some_and(|writer| writer.join().unwrap_or(true));
            #[cfg(unix)]
            use std::os::unix::process::ExitStatusExt;
            let (exit_code, signal) = match result {
                Ok(status) => (if stdin_failed { Some(-1) } else { status.code() }, {
                    #[cfg(unix)] { status.signal() }
                    #[cfg(not(unix))] { None }
                }),
                Err(_) => (None, None),
            };
            let mut inner = run.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.status = RunStatus::Exited;
            inner.exit_code = exit_code;
            inner.signal = signal;
            drop(inner);
            publisher(Event::Exit { run_id: run.id.clone(), incarnation_id: run.incarnation.clone(),
                exit_code, signal });
        });
        Ok(SpawnResult { state, is_new: true })
    }

    pub fn list(&self) -> ListResult {
        let runs = self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        ListResult { runs: runs.values().map(|run| run.snapshot()).collect() }
    }

    pub fn attach(&self, params: AttachParams) -> Option<AttachResult> {
        let run = self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&params.run_id).cloned()?;
        let inner = run.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = run.state(&inner);
        let mut remaining = params.max_bytes.unwrap_or(REPLAY_PAGE_MAX_BYTES)
            .clamp(16384, REPLAY_PAGE_MAX_BYTES);
        let stdout = inner.stdout.page(params.after_stdout_seq, &mut remaining);
        let stderr = inner.stderr.page(params.after_stderr_seq, &mut remaining);
        Some(AttachResult { state, stdout, stderr })
    }

    #[cfg(unix)]
    pub fn interrupt(&self, run_id: &str, signal: &str) -> Result<(), String> {
        let number = match signal {
            "SIGTERM" => libc::SIGTERM, "SIGINT" => libc::SIGINT,
            "SIGKILL" => libc::SIGKILL, "SIGHUP" => libc::SIGHUP,
            _ => return Err("unsupported signal".into()),
        };
        let run = self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id).cloned().ok_or("unknown structured run")?;
        // A zombie retains its PID until the reaper waits; after wait the
        // reaper marks the run exited. Never kill a known exited PID.
        let inner = run.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.status == RunStatus::Running {
            let result = unsafe { libc::kill(run.pid as i32, number) };
            if result != 0 { return Err("failed to signal structured run".into()); }
        }
        Ok(())
    }

    /// A single-run forget never touches another owner. Running children are
    /// interrupted before their records are released.
    pub fn forget(&self, run_id: &str) -> Result<(), String> {
        #[cfg(unix)] self.interrupt(run_id, "SIGTERM")?;
        self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(run_id).map(|_| ()).ok_or_else(|| "unknown structured run".into())
    }

    pub fn begin_drain(&self) { self.draining.store(true, Ordering::SeqCst); }

    pub fn running_count(&self) -> usize {
        self.list().runs.iter().filter(|run| run.status == RunStatus::Running).count()
    }

    #[cfg(unix)]
    pub fn stop_now(&self) {
        self.begin_drain();
        for run in self.list().runs {
            if run.status == RunStatus::Running { let _ = self.interrupt(&run.run_id, "SIGKILL"); }
        }
    }

    pub fn retained_bytes(&self) -> usize {
        self.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .values().map(|run| {
                let inner = run.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                inner.stdout.size + inner.stderr.size
            }).sum()
    }
}

fn read_pipe<R: Read + Send + 'static>(
    mut pipe: R, run: Arc<Run>, stream: StreamName,
    publish: Arc<dyn Fn(Event) + Send + Sync>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut decoder = IncrementalUtf8Decoder::new();
        let mut bytes = [0u8; 8192];
        loop {
            let n = match pipe.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let data = decoder.push(&bytes[..n]);
            if data.is_empty() { continue; }
            let mut inner = run.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let log = if stream == StreamName::Stdout { &mut inner.stdout } else { &mut inner.stderr };
            let seq = log.append(data.clone());
            drop(inner);
            publish(Event::Stream { run_id: run.id.clone(), incarnation_id: run.incarnation.clone(),
                stream, seq, data });
        }
        decoder.discard_tail();
    })
}
