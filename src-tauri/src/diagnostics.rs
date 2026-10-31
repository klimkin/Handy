//! Opt-in diagnostics tracing.
//!
//! Records one Chrome Trace Event file (Perfetto-compatible) per capture,
//! holding only timing evidence: fixed-vocabulary event names, allowlisted
//! outcome codes, counters, and booleans. By construction no user audio,
//! text, clipboard data, local path, or free-form error string can enter a
//! trace; [`Event`] accepts only enums, numbers, and bools.
//!
//! The free functions target the process-global recorder. Tests build
//! their own [`Recorder`] with tiny caps instead.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Bounded queue between emitters and the writer thread.
const QUEUE_CAP: usize = 1024;
/// Hard ceiling on trace bytes per capture (32 MiB).
const BYTE_CAP: u64 = 32 * 1024 * 1024;
/// How long `stop` waits for the writer before giving up on it.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Value stamped into `otherData.schema`.
const SCHEMA: &str = "handy-diagnostics/1";

/// Identifies one dictation run across threads and capture start/stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(NonZeroU64);

impl RunId {
    pub fn as_u64(self) -> u64 {
        self.0.get()
    }
}

/// Chrome trace phase of an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Async span begin (`ph: "b"`).
    Begin,
    /// Async span end (`ph: "e"`).
    End,
    /// Thread-scoped instant (`ph: "i"`, `s: "t"`).
    Instant,
}

/// Fixed event vocabulary. Extend only with reviewer sign-off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Name {
    ShortcutEvent,
    InputReceived,
    InputDequeued,
    Effect,
    Run,
    Recording,
    MicOpen,
    FirstAudioDelivered,
    FirstVadFrame,
    FirstSpeech,
    ReadyObserved,
    CaptureStop,
    ModelWait,
    LiveFinalize,
    BatchDecode,
    PostProcess,
    HistorySave,
    Output,
}

impl Name {
    pub const fn as_str(self) -> &'static str {
        match self {
            Name::ShortcutEvent => "shortcut_event",
            Name::InputReceived => "input_received",
            Name::InputDequeued => "input_dequeued",
            Name::Effect => "effect",
            Name::Run => "run",
            Name::Recording => "recording",
            Name::MicOpen => "mic_open",
            Name::FirstAudioDelivered => "first_audio_delivered",
            Name::FirstVadFrame => "first_vad_frame",
            Name::FirstSpeech => "first_speech",
            Name::ReadyObserved => "ready_observed",
            Name::CaptureStop => "capture_stop",
            Name::ModelWait => "model_wait",
            Name::LiveFinalize => "live_finalize",
            Name::BatchDecode => "batch_decode",
            Name::PostProcess => "post_process",
            Name::HistorySave => "history_save",
            Name::Output => "output",
        }
    }
}

/// Allowlisted outcome and error codes. Mapped from internal enums at emit
/// sites, never from an error `Display` output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Code {
    Start,
    Stop,
    Typed,
    Empty,
    Silent,
    Failed,
    Cancelled,
    NoModel,
    MicrophonePermissionDenied,
    NoInputDevice,
    Unknown,
    RecorderFailed,
    SinkPanicked,
    TranscriptionFailed,
    PasteFailed,
}

impl Code {
    pub const fn as_str(self) -> &'static str {
        match self {
            Code::Start => "start",
            Code::Stop => "stop",
            Code::Typed => "typed",
            Code::Empty => "empty",
            Code::Silent => "silent",
            Code::Failed => "failed",
            Code::Cancelled => "cancelled",
            Code::NoModel => "no_model",
            Code::MicrophonePermissionDenied => "microphone_permission_denied",
            Code::NoInputDevice => "no_input_device",
            Code::Unknown => "unknown",
            Code::RecorderFailed => "recorder_failed",
            Code::SinkPanicked => "sink_panicked",
            Code::TranscriptionFailed => "transcription_failed",
            Code::PasteFailed => "paste_failed",
        }
    }
}

/// Numeric and boolean payload; the only payload an [`Event`] accepts.
/// `Copy` rules out owned strings by construction.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Args {
    pub code: Option<Code>,
    pub count: Option<u64>,
    pub flag: Option<bool>,
}

impl Args {
    /// Args carrying only an outcome code — the common case at span ends.
    pub fn code(code: Code) -> Self {
        Args {
            code: Some(code),
            ..Default::default()
        }
    }
}

/// Emitting thread identity: a small stable id plus an interned
/// code-defined thread name. Resolved once per thread, but only while a
/// capture is active (the public emit methods return before this runs
/// when the recorder is disabled).
#[derive(Clone, Copy)]
struct ThreadTag {
    tid: u64,
    name: Option<&'static str>,
}

static THREAD_IDS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static THREAD_TAG: ThreadTag = ThreadTag::capture();
}

impl ThreadTag {
    fn capture() -> Self {
        let tid = THREAD_IDS.fetch_add(1, Ordering::Relaxed) + 1;
        let name = thread::current().name().map(intern_thread_name);
        ThreadTag { tid, name }
    }
}

fn intern_thread_name(name: &str) -> &'static str {
    // Worker threads churn; distinct names do not. Interning keeps the
    // leak bounded by the number of code-defined thread names.
    static NAMES: LazyLock<Mutex<std::collections::HashSet<&'static str>>> =
        LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
    let mut names = NAMES.lock().expect("diagnostics name interner poisoned");
    if let Some(&leaked) = names.get(name) {
        return leaked;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

fn thread_tag() -> ThreadTag {
    THREAD_TAG.with(|t| *t)
}

/// One trace record.
#[derive(Clone, Copy)]
struct Event {
    at: Instant,
    phase: Phase,
    name: Name,
    run: Option<RunId>,
    thread: ThreadTag,
    args: Args,
}

/// Configuration snapshot embedded in `otherData` at capture start. Strings
/// are allowlisted at construction (catalog ids, `"custom"`, variant names)
/// and never enter an [`Event`].
#[derive(Clone, Debug, Serialize)]
pub struct ConfigSnapshot {
    pub model: String,
    /// Decode mode the start path would choose: "stream", "chunked", or
    /// "none" — not just the model capability, so chunked live decode is
    /// not recorded as batch.
    pub live_decode: String,
    pub paste_method: String,
    pub push_to_talk: bool,
    pub post_process_enabled: bool,
    pub vad: bool,
}

/// Live capture status, polled by the UI while a capture runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub active: bool,
    pub started_at_ms: Option<u64>,
    pub events: u64,
    pub dropped: u64,
}

/// Errors from `start`/`stop`.
#[derive(Debug)]
pub enum DiagError {
    Io(std::io::Error),
    /// `stop` was called while no capture was active.
    NotActive,
    /// The writer did not finish within the stop timeout.
    StopTimedOut,
    /// The writer ended without reporting success.
    WriterFailed,
}

impl std::fmt::Display for DiagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiagError::Io(e) => write!(f, "diagnostics io error: {e}"),
            DiagError::NotActive => write!(f, "no diagnostics capture is active"),
            DiagError::StopTimedOut => write!(f, "diagnostics writer did not stop in time"),
            DiagError::WriterFailed => write!(f, "diagnostics writer exited unexpectedly"),
        }
    }
}

impl std::error::Error for DiagError {}

/// State present only while a capture is active. Emitters read it with
/// `try_read`; `stop` takes the write side to drop the sender, which is the
/// writer's drain-and-finish signal.
struct Capture {
    sender: SyncSender<Event>,
    epoch: Instant,
    started_at_ms: u64,
    /// Per-capture counters: every capture starts from zero so the file
    /// and status describe this capture's losses, not the session's.
    admitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

/// The recorder. The process-global instance is a [`LazyLock`]; tests build
/// their own with tiny caps.
pub struct Recorder {
    active: AtomicBool,
    capture: RwLock<Option<Capture>>,
    /// Serializes `start`/`stop` control flow.
    control: Mutex<()>,
    /// Takes the writer's final report (the finished file path).
    done_rx: Mutex<Option<mpsc::Receiver<PathBuf>>>,
    next_id: AtomicU64,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub fn new() -> Self {
        Recorder {
            active: AtomicBool::new(false),
            capture: RwLock::new(None),
            control: Mutex::new(()),
            done_rx: Mutex::new(None),
            next_id: AtomicU64::new(0),
        }
    }

    /// Allocates a run id. Always succeeds, even while inactive, so ids stay
    /// stable across capture start/stop.
    pub fn next_run(&self) -> RunId {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        RunId(NonZeroU64::new(n).expect("counter starts at 1"))
    }

    pub fn begin(&self, run: RunId, name: Name) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        self.emit(Event {
            at: Instant::now(),
            phase: Phase::Begin,
            name,
            run: Some(run),
            thread: thread_tag(),
            args: Args::default(),
        });
    }

    pub fn end(&self, run: RunId, name: Name, args: Args) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        self.emit(Event {
            at: Instant::now(),
            phase: Phase::End,
            name,
            run: Some(run),
            thread: thread_tag(),
            args,
        });
    }

    pub fn instant(&self, run: Option<RunId>, name: Name, args: Args) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        self.emit(Event {
            at: Instant::now(),
            phase: Phase::Instant,
            name,
            run,
            thread: thread_tag(),
            args,
        });
    }

    pub fn instant_at(&self, at: Instant, run: Option<RunId>, name: Name, args: Args) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        self.emit(Event {
            at,
            phase: Phase::Instant,
            name,
            run,
            thread: thread_tag(),
            args,
        });
    }

    /// Slow path, only reached while active. Never blocks on disk, queue
    /// capacity, or locks. A full queue or a pre-epoch timestamp counts
    /// the event as dropped on the live capture's counter; drops in the
    /// stop race (lock contended or sender already gone) go uncounted,
    /// since that capture is finalizing anyway.
    fn emit(&self, ev: Event) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        let guard = match self.capture.try_read() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(cap) = guard.as_ref() else {
            return;
        };
        if ev.at < cap.epoch {
            // Timestamps from before the capture epoch are unusable;
            // reject rather than clamp them to zero.
            cap.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match cap.sender.try_send(ev) {
            Ok(()) => {
                cap.admitted.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                cap.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> Status {
        let guard = self
            .capture
            .read()
            .expect("diagnostics capture lock poisoned");
        let live = guard.as_ref().map(|cap| {
            (
                cap.started_at_ms,
                cap.admitted.load(Ordering::Relaxed),
                cap.dropped.load(Ordering::Relaxed),
            )
        });
        Status {
            active: self.is_active(),
            started_at_ms: live.map(|l| l.0),
            events: live.map_or(0, |l| l.1),
            dropped: live.map_or(0, |l| l.2),
        }
    }

    pub fn start(&self, dir: &Path, config: ConfigSnapshot) -> Result<Status, DiagError> {
        self.start_inner(dir, config, QUEUE_CAP, BYTE_CAP)
    }

    fn start_inner(
        &self,
        dir: &Path,
        config: ConfigSnapshot,
        queue_cap: usize,
        byte_cap: u64,
    ) -> Result<Status, DiagError> {
        let _control = self
            .control
            .lock()
            .expect("diagnostics control lock poisoned");
        if self.active.load(Ordering::Relaxed) {
            return Ok(self.status());
        }
        fs::create_dir_all(dir).map_err(DiagError::Io)?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
        let mut n: u32 = 0;
        let (partial, final_path) = loop {
            let tag = if n == 0 {
                stamp.clone()
            } else {
                format!("{stamp}-{n}")
            };
            let partial = dir.join(format!("trace-{tag}.json.partial"));
            if !partial.exists() {
                break (partial, dir.join(format!("trace-{tag}.json")));
            }
            n += 1;
        };
        let file = File::create(&partial).map_err(DiagError::Io)?;
        let orphan = partial.clone();
        let (tx, rx) = sync_channel(queue_cap);
        let (done_tx, done_rx) = mpsc::channel();
        let epoch = Instant::now();
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let admitted = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));

        *self.done_rx.lock().expect("diagnostics done lock poisoned") = Some(done_rx);
        *self
            .capture
            .write()
            .expect("diagnostics capture lock poisoned") = Some(Capture {
            sender: tx,
            epoch,
            started_at_ms,
            admitted: Arc::clone(&admitted),
            dropped: Arc::clone(&dropped),
        });
        self.active.store(true, Ordering::Release);

        let writer = Writer {
            rx,
            done_tx,
            out: BufWriter::new(file),
            partial,
            final_path,
            epoch,
            byte_cap,
            dropped,
            config,
        };
        let spawn = thread::Builder::new()
            .name("diagnostics-writer".to_string())
            .spawn(move || writer.run());
        if let Err(e) = spawn {
            // Roll back to idle so a later start can retry cleanly.
            self.active.store(false, Ordering::Release);
            *self
                .capture
                .write()
                .expect("diagnostics capture lock poisoned") = None;
            *self.done_rx.lock().expect("diagnostics done lock poisoned") = None;
            let _ = fs::remove_file(orphan);
            return Err(DiagError::Io(e));
        }
        Ok(self.status())
    }

    /// Stops the capture. Drains every event already admitted, marks spans
    /// still open as incomplete, finalizes the file, and returns its path.
    /// Never waits on transcription or paste; those spans simply end as
    /// incomplete.
    pub fn stop(&self) -> Result<PathBuf, DiagError> {
        let _control = self
            .control
            .lock()
            .expect("diagnostics control lock poisoned");
        let rx = self
            .done_rx
            .lock()
            .expect("diagnostics done lock poisoned")
            .take();
        let Some(rx) = rx else {
            return Err(DiagError::NotActive);
        };
        self.active.store(false, Ordering::Release);
        *self
            .capture
            .write()
            .expect("diagnostics capture lock poisoned") = None;
        match rx.recv_timeout(STOP_TIMEOUT) {
            Ok(path) => Ok(path),
            Err(RecvTimeoutError::Timeout) => Err(DiagError::StopTimedOut),
            Err(RecvTimeoutError::Disconnected) => Err(DiagError::WriterFailed),
        }
    }
}

/// Owns the trace file between `start` and `stop`.
struct Writer {
    rx: Receiver<Event>,
    done_tx: mpsc::Sender<PathBuf>,
    out: BufWriter<File>,
    partial: PathBuf,
    final_path: PathBuf,
    epoch: Instant,
    byte_cap: u64,
    dropped: Arc<AtomicU64>,
    config: ConfigSnapshot,
}

impl Writer {
    fn run(mut self) {
        let header = b"{\"traceEvents\":[\n";
        let mut bytes: u64 = header.len() as u64;
        let mut truncated: u64 = 0;
        // Cleared on any write error. A file that failed mid-stream cannot
        // be trusted to parse, so it must not reach the caller as success.
        let mut io_ok = true;
        // Open async spans keyed by (run, name), holding the thread the span
        // began on so the incomplete end lands on an honest track.
        let mut open: BTreeMap<(RunId, Name), u64> = BTreeMap::new();
        let mut threads: BTreeMap<u64, Option<&'static str>> = BTreeMap::new();

        io_ok &= self.out.write_all(header).is_ok();
        let mut first = true;
        for ev in self.rx {
            threads.entry(ev.thread.tid).or_insert(ev.thread.name);
            let Some(line) = event_line(&ev, self.epoch, false) else {
                truncated += 1;
                continue;
            };
            let sep_len = u64::from(!first);
            if bytes + line.len() as u64 + sep_len + 1 > self.byte_cap {
                truncated += 1;
                continue;
            }
            let ok = (first || self.out.write_all(b",").is_ok())
                && self.out.write_all(line.as_bytes()).is_ok()
                && self.out.write_all(b"\n").is_ok();
            if !ok {
                io_ok = false;
                log::warn!("diagnostics writer: file write failed; stopping capture early");
                break;
            }
            first = false;
            bytes += line.len() as u64 + sep_len + 1;
            match ev.phase {
                Phase::Begin => {
                    if let Some(run) = ev.run {
                        open.insert((run, ev.name), ev.thread.tid);
                    }
                }
                Phase::End => {
                    if let Some(run) = ev.run {
                        open.remove(&(run, ev.name));
                    }
                }
                Phase::Instant => {}
            }
        }

        // The sender is gone: every admitted event has been handled. End
        // still-open spans at the drain moment rather than losing them.
        let stop_at = Instant::now();
        let mut incomplete: u64 = 0;
        for ((run, name), tid) in open {
            let ev = Event {
                at: stop_at,
                phase: Phase::End,
                name,
                run: Some(run),
                thread: ThreadTag {
                    tid,
                    name: threads.get(&tid).copied().flatten(),
                },
                args: Args::default(),
            };
            if let Some(line) = event_line(&ev, self.epoch, true) {
                io_ok &= (first || self.out.write_all(b",").is_ok())
                    && self.out.write_all(line.as_bytes()).is_ok()
                    && self.out.write_all(b"\n").is_ok();
                first = false;
                incomplete += 1;
            }
        }

        // Metadata: process and thread names.
        io_ok &= (first || self.out.write_all(b",").is_ok())
            && self.out.write_all(meta_process_line().as_bytes()).is_ok()
            && self.out.write_all(b"\n").is_ok();
        for (tid, name) in &threads {
            io_ok &= self.out.write_all(b",").is_ok()
                && self
                    .out
                    .write_all(meta_thread_line(*tid, *name).as_bytes())
                    .is_ok()
                && self.out.write_all(b"\n").is_ok();
        }

        let other = serde_json::json!({
            "schema": SCHEMA,
            "app_version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "capture_coverage": "unknown",
            "first_audio_semantics": "delivery_latency",
            "output_semantics": "handy_call_returned",
            "dropped_events": self.dropped.load(Ordering::Relaxed),
            "truncated_events": truncated,
            "incomplete_spans": incomplete,
            "config": &self.config,
        });
        io_ok &= self
            .out
            .write_all(format!("],\"otherData\":{other}}}\n").as_bytes())
            .is_ok();
        if let Err(e) = self.out.flush() {
            io_ok = false;
            log::warn!("diagnostics writer: flush failed: {e}");
        }
        if let Err(e) = self.out.get_ref().sync_all() {
            io_ok = false;
            log::warn!("diagnostics writer: fsync failed: {e}");
        }

        if !io_ok {
            // Half-written JSON must not reach the caller as a success.
            // Dropping done_tx without sending makes stop report
            // WriterFailed; the partial file is not worth keeping.
            drop(self.done_tx);
            let _ = fs::remove_file(&self.partial);
            return;
        }

        let mut done_path = self.final_path.clone();
        if let Err(e) = fs::rename(&self.partial, &self.final_path) {
            log::warn!(
                "diagnostics writer: rename to {} failed: {e}",
                self.final_path.display()
            );
            done_path = self.partial.clone();
        }
        let _ = self.done_tx.send(done_path);
    }
}

fn meta_process_line() -> String {
    "{\"ph\":\"M\",\"name\":\"process_name\",\"pid\":1,\"args\":{\"name\":\"Handy\"}}".to_string()
}

fn meta_thread_line(tid: u64, name: Option<&str>) -> String {
    let label = name.map_or_else(|| format!("thread-{tid}"), str::to_string);
    let escaped = serde_json::to_string(&label).unwrap_or_else(|_| "\"?\"".to_string());
    let mut s = String::with_capacity(64);
    s.push_str("{\"ph\":\"M\",\"name\":\"thread_name\",\"pid\":1,\"tid\":");
    push_u64(&mut s, tid);
    s.push_str(",\"args\":{\"name\":");
    s.push_str(&escaped);
    s.push_str("}}");
    s
}

/// Serializes one event. All string content is fixed vocabulary, so no JSON
/// escaping is needed. Returns `None` for timestamps before the epoch.
fn event_line(ev: &Event, epoch: Instant, incomplete: bool) -> Option<String> {
    let ts = ev.at.checked_duration_since(epoch)?;
    let mut s = String::with_capacity(96);
    s.push_str("{\"ph\":");
    match ev.phase {
        Phase::Begin => s.push_str("\"b\",\"cat\":\"run\""),
        Phase::End => s.push_str("\"e\",\"cat\":\"run\""),
        Phase::Instant => s.push_str("\"i\",\"s\":\"t\""),
    }
    s.push_str(",\"name\":\"");
    s.push_str(ev.name.as_str());
    s.push('"');
    if let Some(run) = ev.run {
        s.push_str(",\"id\":");
        push_u64(&mut s, run.as_u64());
    }
    s.push_str(",\"ts\":");
    push_u64(&mut s, ts.as_micros() as u64);
    s.push_str(",\"pid\":1,\"tid\":");
    push_u64(&mut s, ev.thread.tid);
    let a = &ev.args;
    if a.code.is_some() || a.count.is_some() || a.flag.is_some() || incomplete {
        s.push_str(",\"args\":{");
        let mut first = true;
        if let Some(c) = a.code {
            s.push_str("\"code\":\"");
            s.push_str(c.as_str());
            s.push('"');
            first = false;
        }
        if let Some(n) = a.count {
            if !first {
                s.push(',');
            }
            s.push_str("\"count\":");
            push_u64(&mut s, n);
            first = false;
        }
        if let Some(f) = a.flag {
            if !first {
                s.push(',');
            }
            s.push_str("\"flag\":");
            s.push_str(if f { "true" } else { "false" });
            first = false;
        }
        if incomplete {
            if !first {
                s.push(',');
            }
            s.push_str("\"incomplete\":true");
        }
        s.push('}');
    }
    s.push('}');
    Some(s)
}

fn push_u64(s: &mut String, n: u64) {
    use std::fmt::Write as _;
    let _ = write!(s, "{n}");
}

static RECORDER: LazyLock<Recorder> = LazyLock::new(Recorder::new);

pub fn next_run() -> RunId {
    RECORDER.next_run()
}

pub fn begin(run: RunId, name: Name) {
    RECORDER.begin(run, name);
}

pub fn end(run: RunId, name: Name, args: Args) {
    RECORDER.end(run, name, args);
}

pub fn instant(run: Option<RunId>, name: Name, args: Args) {
    RECORDER.instant(run, name, args);
}

pub fn instant_at(at: Instant, run: Option<RunId>, name: Name, args: Args) {
    RECORDER.instant_at(at, run, name, args);
}

pub fn is_active() -> bool {
    RECORDER.is_active()
}

pub fn start(dir: &Path, config: ConfigSnapshot) -> Result<Status, DiagError> {
    RECORDER.start(dir, config)
}

pub fn stop() -> Result<PathBuf, DiagError> {
    RECORDER.stop()
}

pub fn status() -> Status {
    RECORDER.status()
}

// Single-source variant lists for the privacy test. The size assertions in
// `vocabulary_lists_are_complete` fail when a variant is added without
// extending these lists.
#[cfg(test)]
const NAME_LIST: &[Name] = &[
    Name::ShortcutEvent,
    Name::InputReceived,
    Name::InputDequeued,
    Name::Effect,
    Name::Run,
    Name::Recording,
    Name::MicOpen,
    Name::FirstAudioDelivered,
    Name::FirstVadFrame,
    Name::FirstSpeech,
    Name::ReadyObserved,
    Name::CaptureStop,
    Name::ModelWait,
    Name::LiveFinalize,
    Name::BatchDecode,
    Name::PostProcess,
    Name::HistorySave,
    Name::Output,
];

#[cfg(test)]
const CODE_LIST: &[Code] = &[
    Code::Start,
    Code::Stop,
    Code::Typed,
    Code::Empty,
    Code::Silent,
    Code::Failed,
    Code::Cancelled,
    Code::NoModel,
    Code::MicrophonePermissionDenied,
    Code::NoInputDevice,
    Code::Unknown,
    Code::RecorderFailed,
    Code::SinkPanicked,
    Code::TranscriptionFailed,
    Code::PasteFailed,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg() -> ConfigSnapshot {
        ConfigSnapshot {
            model: "whisper-small".to_string(),
            live_decode: "none".to_string(),
            paste_method: "None".to_string(),
            push_to_talk: false,
            post_process_enabled: false,
            vad: true,
        }
    }

    fn start(rec: &Recorder, dir: &Path) {
        rec.start_inner(dir, cfg(), 1024, BYTE_CAP).unwrap();
    }

    fn parse(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn non_meta_events(v: &serde_json::Value) -> Vec<&serde_json::Value> {
        v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"].as_str() != Some("M"))
            .collect()
    }

    #[test]
    fn start_stop_roundtrip() {
        let rec = Recorder::new();
        let dir = tempfile::tempdir().unwrap();
        start(&rec, dir.path());
        let run = rec.next_run();
        rec.begin(run, Name::Run);
        rec.instant(Some(run), Name::FirstAudioDelivered, Args::default());
        rec.end(
            run,
            Name::Run,
            Args {
                code: Some(Code::Typed),
                ..Default::default()
            },
        );
        let path = rec.stop().unwrap();
        let v = parse(&path);

        let evs = non_meta_events(&v);
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0]["ph"], "b");
        assert_eq!(evs[0]["name"], "run");
        assert_eq!(evs[0]["id"].as_u64(), Some(run.as_u64()));
        assert_eq!(evs[1]["ph"], "i");
        assert_eq!(evs[1]["name"], "first_audio_delivered");
        assert_eq!(evs[2]["ph"], "e");
        assert_eq!(evs[2]["args"]["code"], "typed");

        // ts is non-decreasing per thread.
        let mut last_by_tid: HashMap<u64, u64> = HashMap::new();
        for e in &evs {
            let tid = e["tid"].as_u64().unwrap();
            let ts = e["ts"].as_u64().unwrap();
            if let Some(prev) = last_by_tid.get(&tid) {
                assert!(ts >= *prev, "ts went backwards on thread {tid}");
            }
            last_by_tid.insert(tid, ts);
        }

        assert_eq!(
            v["otherData"]["schema"].as_str().unwrap(),
            "handy-diagnostics/1"
        );
    }

    #[test]
    fn inactive_records_nothing() {
        let rec = Recorder::new();
        for _ in 0..1000 {
            rec.instant(None, Name::Effect, Args::default());
        }
        let dir = tempfile::tempdir().unwrap();
        start(&rec, dir.path());
        let path = rec.stop().unwrap();
        let v = parse(&path);
        assert!(non_meta_events(&v).is_empty());
    }

    #[test]
    fn saturation_truncates_and_still_parses() {
        let rec = Recorder::new();
        let dir = tempfile::tempdir().unwrap();
        rec.start_inner(dir.path(), cfg(), 1024, 300).unwrap();
        for _ in 0..50 {
            rec.begin(rec.next_run(), Name::MicOpen);
        }
        let path = rec.stop().unwrap();
        let v = parse(&path);
        let other = &v["otherData"];
        assert!(
            other["truncated_events"].as_u64().unwrap() > 0
                || other["dropped_events"].as_u64().unwrap() > 0
        );
        assert!(v["traceEvents"].as_array().unwrap().len() < 50);
    }

    #[test]
    fn counters_reset_between_captures() {
        let rec = Recorder::new();
        let dir = tempfile::tempdir().unwrap();
        rec.start_inner(dir.path(), cfg(), 1024, 300).unwrap();
        // One deterministic drop (pre-epoch) on top of byte-cap truncation,
        // so the reset assertions below exercise both counters.
        rec.instant_at(
            Instant::now() - Duration::from_secs(5),
            None,
            Name::Effect,
            Args::default(),
        );
        for _ in 0..50 {
            rec.begin(rec.next_run(), Name::MicOpen);
        }
        let path1 = rec.stop().unwrap();
        assert_eq!(
            parse(&path1)["otherData"]["dropped_events"].as_u64(),
            Some(1)
        );

        start(&rec, dir.path());
        assert_eq!(rec.status().dropped, 0);
        let run = rec.next_run();
        rec.begin(run, Name::Run);
        rec.instant(Some(run), Name::Effect, Args::default());
        rec.end(run, Name::Run, Args::default());
        let path2 = rec.stop().unwrap();
        let v = parse(&path2);
        let other = &v["otherData"];
        assert_eq!(other["dropped_events"].as_u64(), Some(0));
        assert_eq!(other["truncated_events"].as_u64(), Some(0));
        assert_eq!(non_meta_events(&v).len(), 3);
    }

    #[test]
    fn open_span_marked_incomplete() {
        let rec = Recorder::new();
        let dir = tempfile::tempdir().unwrap();
        start(&rec, dir.path());
        let run = rec.next_run();
        rec.begin(run, Name::Run);
        rec.begin(run, Name::Recording);
        let path = rec.stop().unwrap();
        let v = parse(&path);
        let evs = non_meta_events(&v);
        let ends: Vec<_> = evs
            .iter()
            .filter(|e| e["ph"] == "e" && e["args"]["incomplete"] == true)
            .collect();
        assert_eq!(ends.len(), 2);
        assert_eq!(v["otherData"]["incomplete_spans"].as_u64(), Some(2));
    }

    #[test]
    fn double_start_double_stop() {
        let rec = Recorder::new();
        let dir = tempfile::tempdir().unwrap();
        start(&rec, dir.path());
        let second = rec.start_inner(dir.path(), cfg(), 1024, BYTE_CAP).unwrap();
        assert!(second.active);
        let path = rec.stop().unwrap();
        assert!(matches!(rec.stop(), Err(DiagError::NotActive)));
        let mut files: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], path);
    }

    #[test]
    fn vocabulary_lists_are_complete() {
        assert_eq!(NAME_LIST.len(), 18);
        assert_eq!(CODE_LIST.len(), 15);
    }

    #[test]
    fn fixed_vocabulary_is_private() {
        let mut blob = String::new();
        for n in NAME_LIST {
            blob.push_str(n.as_str());
        }
        for c in CODE_LIST {
            blob.push_str(c.as_str());
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(!blob.contains('/'));
        assert!(!blob.contains('\\'));
        assert!(!blob.contains(dir.path().to_str().unwrap()));

        // Compile-level shape: payload and event cannot own heap strings
        // (String and PathBuf are not Copy). A borrowed `&'static str`
        // would still satisfy Copy; the vocabulary assertions above are
        // what pin down content.
        fn is_copy<T: Copy>() {}
        is_copy::<Args>();
        is_copy::<Event>();
    }

    #[test]
    fn pre_epoch_event_rejected() {
        let rec = Recorder::new();
        let dir = tempfile::tempdir().unwrap();
        start(&rec, dir.path());
        let run = rec.next_run();
        let past = Instant::now() - Duration::from_secs(5);
        rec.instant_at(past, Some(run), Name::InputReceived, Args::default());
        rec.instant(Some(run), Name::InputDequeued, Args::default());
        let path = rec.stop().unwrap();
        let v = parse(&path);
        let evs = non_meta_events(&v);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["name"], "input_dequeued");
        assert_eq!(v["otherData"]["dropped_events"].as_u64(), Some(1));
    }
}
