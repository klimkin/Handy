use super::StatsManager;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

const CONFIG_FIELDS: &[&str] = &[
    "model",
    "requested_model",
    "selected_language",
    "spoken_languages",
    "translate_to_english",
    "accelerator",
    "gpu_device",
    "vad_enabled",
    "vad_backend",
    "long_form_chunking",
    "lazy_stream_close",
    "always_on_microphone",
    "shortcut_activation",
    "language_detection",
    "model_unload_timeout",
    "post_process",
    "paste_method",
    "paste_delay_ms",
    "paste_delay_after_ms",
    "extra_recording_buffer_ms",
    "post_process_provider",
    "post_process_model",
    "post_process_prompt",
    "post_process_prompt_sha256",
    "auto_submit",
    "auto_submit_key",
    "clipboard_handling",
    "reliable_paste",
    "append_trailing_space",
    "typing_tool",
];
fn bounded_identifier(value: &str) -> (String, bool) {
    if value.len() <= 128 {
        return (value.into(), false);
    }
    use sha2::{Digest, Sha256};
    let mut boundary = 63;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (
        format!(
            "{}#{:x}",
            &value[..boundary],
            Sha256::digest(value.as_bytes())
        ),
        true,
    )
}
const EVENT_BUDGET: usize = 192;

#[derive(Clone, Debug)]
pub struct InputStamp {
    pub run_id: String,
    pub at: Instant,
    pub utc: String,
    pub origin: InputOrigin,
    pub generation: Option<u64>,
    pub mode: Option<crate::settings::ShortcutActivation>,
}
impl InputStamp {
    pub fn now(origin: InputOrigin) -> Self {
        Self::at(Instant::now(), origin)
    }
    pub fn at(at: Instant, origin: InputOrigin) -> Self {
        let utc = chrono::Utc::now();
        Self {
            run_id: uuid::Uuid::new_v4().to_string(),
            at,
            utc: utc.to_rfc3339(),
            origin,
            generation: None,
            mode: None,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputOrigin {
    ShortcutCallback,
    ExternalCommand,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    ModelLoad,
    VadPrepare,
    MicrophoneOpen,
    LiveAsr,
    BatchAsr,
    LiveFinalize,
    WavWrite,
    WavVerify,
    HistoryWrite,
    PostProcess,
    PostProcessAttempt,
    Output,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseOutcome {
    Success,
    Failed,
    Cancelled,
    Unavailable,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Model,
    Vad,
    Microphone,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceReadiness {
    Ready,
    Cold,
    Loading,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum Event {
    ResourceState {
        resource: ResourceKind,
        state: ResourceReadiness,
    },
    Activation,
    StopInput,
    RecordingRequested,
    CaptureStopped,
    FirstRawBlockObserved,
    FirstVadOutput {
        raw_first: Option<u64>,
        valid_frames: Option<u64>,
    },
    PhaseStarted {
        phase_id: u64,
        kind: PhaseKind,
    },
    PhaseCompleted {
        phase_id: u64,
        outcome: PhaseOutcome,
    },
    AsrFallback,
    PostProcessFallback,
    PostProcessSkipped {
        reason: SkipReason,
    },
    PasteInjected,
    OutputStarted,
    CancelRequested,
    OutputDone,
    Terminal {
        outcome: TerminalOutcome,
    },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    NotRequested,
    EmptyInput,
    ProviderUnavailable,
    ModelUnavailable,
    PromptUnavailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    OutputCompleted,
    OutputUnconfirmed,
    OutputFailed,
    NoOutputRequested,
    Empty,
    Cancelled,
    CaptureFailed,
    ProcessingFailed,
    UnfinishedOwner,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    ClipboardPaste,
    ClipboardOnly,
    DirectTyping,
    ExternalScript,
    None,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionBasis {
    ClipboardWrite,
    SynchronousOutputReturn,
    DirectTypingReturn,
    ScriptProcessExit,
    ReceiptTransactionSettled,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnconfirmedReason {
    Timeout,
    OwnershipLost,
    Superseded,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFailure {
    Injection,
    Typing,
    Script,
    Restore,
    AutoSubmit,
    Disconnected,
    Other,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum OutputResult {
    Completed(CompletionBasis),
    Unconfirmed(UnconfirmedReason),
    Failed(OutputFailure),
    NotRequested,
}
#[derive(Clone, Copy, Debug)]
pub struct OutputSettlement {
    pub at: Instant,
    pub kind: OutputKind,
    pub result: OutputResult,
}
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub enum OutputReceipt {
    Ready(OutputSettlement),
    Pending {
        receiver: mpsc::Receiver<OutputSettlement>,
        kind: OutputKind,
    },
}
impl OutputReceipt {
    pub fn immediate(settlement: OutputSettlement) -> Self {
        Self::Ready(settlement)
    }
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub fn pending(receiver: mpsc::Receiver<OutputSettlement>, kind: OutputKind) -> Self {
        Self::Pending { receiver, kind }
    }
    pub fn wait(self) -> OutputSettlement {
        match self {
            Self::Ready(settlement) => settlement,
            Self::Pending { receiver, kind } => receiver.recv().unwrap_or(OutputSettlement {
                at: Instant::now(),
                kind,
                result: OutputResult::Failed(OutputFailure::Disconnected),
            }),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct CaptureEvidence {
    pub first_delivery: Option<Instant>,
    pub first_vad: Option<Instant>,
    pub first_vad_range: Option<(u64, u64)>,
    pub valid_frames: u64,
    pub dropped_frames: u64,
    pub capture_failed: bool,
    pub speech_detected: bool,
    pub measured: Option<MeasuredCoverage>,
}
#[derive(Clone, Debug)]
pub struct MeasuredCoverage {
    pub first: Instant,
    pub through: Instant,
    pub uncertainty: Duration,
    pub discontinuities: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metric {
    pub us: Option<i64>,
    pub reason: Option<String>,
    pub uncertainty_us: Option<u64>,
}
impl Metric {
    fn unavailable(reason: &str) -> Self {
        Self {
            us: None,
            reason: Some(reason.into()),
            uncertainty_us: None,
        }
    }
    fn measured(us: i64, uncertainty_us: u64) -> Self {
        Self {
            us: Some(us),
            reason: None,
            uncertainty_us: Some(uncertainty_us),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metrics {
    pub capture_gap: Metric,
    pub output_gap: Metric,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub seq: u64,
    pub t_us: i64,
    #[serde(flatten)]
    pub event: Event,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureRecord {
    pub timing_source: String,
    pub reason: Option<String>,
    pub timing_uncertainty_us: Option<u64>,
    pub first_retained_sample_us: Option<i64>,
    pub retained_through_us: Option<i64>,
    pub first_delivery_us: Option<i64>,
    pub first_vad_output_us: Option<i64>,
    pub vad_source_range: Option<(u64, u64)>,
    pub valid_frames: u64,
    pub dropped_frames: u64,
    pub discontinuities: Option<u64>,
    pub capture_failed: bool,
    pub speech_detected: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputRecord {
    pub completion_basis: Option<CompletionBasis>,
    pub kind: OutputKind,
    pub result: OutputResult,
    pub settled_us: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PostProcessRecord {
    pub requested: bool,
    pub attempted: Option<bool>,
    pub attempts: u32,
    pub outcome: String,
    pub skipped_reason: Option<SkipReason>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinalizedTrace {
    pub schema_version: u32,
    pub run_id: String,
    pub app_version: String,
    pub started_at_utc: String,
    pub origin: InputOrigin,
    pub config: BTreeMap<String, String>,
    pub language: Option<String>,
    pub outcome: TerminalOutcome,
    pub capture: CaptureRecord,
    pub output: Option<OutputRecord>,
    pub post_process: PostProcessRecord,
    pub metadata_truncated: bool,
    pub events: Vec<NormalizedEvent>,
    pub metrics: Metrics,
    pub events_complete: bool,
    pub dropped_events: u64,
}
struct StampedEvent {
    seq: u64,
    at: Instant,
    event: Event,
}
struct Diagnostics {
    capture: Mutex<CaptureEvidence>,
    events: Mutex<Vec<StampedEvent>>,
    next: AtomicU64,
    dropped: AtomicU64,
    frozen: std::sync::atomic::AtomicBool,
}
#[derive(Clone)]
pub struct TraceReporter(Arc<Diagnostics>);
impl TraceReporter {
    /// Publish completed capture evidence outside the audio callback.
    pub fn captured(&self, evidence: CaptureEvidence) {
        *self.0.capture.lock().unwrap_or_else(|e| e.into_inner()) = evidence;
    }

    pub fn event(&self, at: Instant, event: Event) {
        if self.0.frozen.load(Ordering::Acquire) {
            return;
        }
        let seq = self.0.next.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut events) = self.0.events.try_lock() {
            if self.0.frozen.load(Ordering::Acquire) {
                return;
            }
            if events.len() < EVENT_BUDGET {
                events.push(StampedEvent { seq, at, event });
                return;
            }
        }
        self.0.dropped.fetch_add(1, Ordering::Relaxed);
    }
    pub fn begin(&self, kind: PhaseKind) -> u64 {
        self.begin_at(kind, Instant::now())
    }
    pub fn begin_at(&self, kind: PhaseKind, at: Instant) -> u64 {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        self.event(at, Event::PhaseStarted { phase_id: id, kind });
        id
    }
    pub fn end(&self, id: u64, outcome: PhaseOutcome) {
        self.event(
            Instant::now(),
            Event::PhaseCompleted {
                phase_id: id,
                outcome,
            },
        );
    }
}
pub struct RunOwner {
    pub config: BTreeMap<String, String>,
    pub language: Option<String>,
    activation: InputStamp,
    stop: Option<(Instant, u64)>,
    diagnostics: TraceReporter,
    stats: Arc<StatsManager>,
    generation: u64,
    finished: bool,
}
impl RunOwner {
    pub(super) fn new(
        stats: Arc<StatsManager>,
        activation: InputStamp,
        config: BTreeMap<String, String>,
        generation: u64,
    ) -> Self {
        stats.owner_started();
        let language = config
            .get("selected_language")
            .filter(|s| s.as_str() != "auto")
            .cloned();
        Self {
            config,
            language,
            activation,
            stop: None,
            diagnostics: TraceReporter(Arc::new(Diagnostics {
                capture: Mutex::new(CaptureEvidence::default()),
                events: Mutex::new(Vec::with_capacity(EVENT_BUDGET)),
                next: AtomicU64::new(4),
                dropped: AtomicU64::new(0),
                frozen: std::sync::atomic::AtomicBool::new(false),
            })),
            stats,
            generation,
            finished: false,
        }
    }
    pub fn reporter(&self) -> TraceReporter {
        self.diagnostics.clone()
    }
    pub fn set_stop(&mut self, at: Instant) {
        if self.stop.is_none() {
            self.stop = Some((at, self.diagnostics.0.next.fetch_add(1, Ordering::Relaxed)));
        }
    }
    pub fn captured(&mut self, evidence: CaptureEvidence) {
        self.diagnostics.captured(evidence);
    }
    pub fn finish(
        mut self,
        outcome: TerminalOutcome,
        at: Instant,
        output: Option<OutputSettlement>,
    ) {
        self.finished = true;
        let trace = self.finalize(outcome, at, output);
        self.stats.record_trace(&trace, self.generation);
    }
    pub(super) fn finalize(
        &self,
        outcome: TerminalOutcome,
        at: Instant,
        output: Option<OutputSettlement>,
    ) -> FinalizedTrace {
        self.diagnostics.0.frozen.store(true, Ordering::Release);
        let t0 = self.activation.at;
        let capture = self
            .diagnostics
            .0
            .capture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut events = self
            .diagnostics
            .0
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| NormalizedEvent {
                seq: e.seq,
                t_us: relative_us(e.at, t0),
                event: e.event,
            })
            .collect::<Vec<_>>();
        events.push(NormalizedEvent {
            seq: 0,
            t_us: 0,
            event: Event::Activation,
        });
        if let Some((stop, seq)) = self.stop {
            events.push(NormalizedEvent {
                seq,
                t_us: relative_us(stop, t0),
                event: Event::StopInput,
            });
        }
        if let Some(done) = output.filter(|o| matches!(o.result, OutputResult::Completed(_))) {
            events.push(NormalizedEvent {
                seq: self.diagnostics.0.next.fetch_add(1, Ordering::Relaxed),
                t_us: relative_us(done.at, t0),
                event: Event::OutputDone,
            });
        }
        events.push(NormalizedEvent {
            seq: self.diagnostics.0.next.fetch_add(1, Ordering::Relaxed),
            t_us: relative_us(at, t0),
            event: Event::Terminal { outcome },
        });
        if let Some(delivery) = capture.first_delivery {
            events.push(NormalizedEvent {
                seq: u64::MAX - 1,
                t_us: relative_us(delivery, t0),
                event: Event::FirstRawBlockObserved,
            });
        }
        if let Some(vad) = capture.first_vad {
            events.push(NormalizedEvent {
                seq: u64::MAX,
                t_us: relative_us(vad, t0),
                event: Event::FirstVadOutput {
                    raw_first: capture.first_vad_range.map(|r| r.0),
                    valid_frames: capture.first_vad_range.map(|r| r.1),
                },
            });
        }
        events.sort_by_key(|e| (e.t_us, e.seq));
        let measured = capture.measured.as_ref();
        let capture = CaptureRecord {
            timing_source: if measured.is_some() {
                "validated_sample_clock"
            } else {
                "unavailable"
            }
            .into(),
            reason: measured
                .is_none()
                .then(|| "sample_timestamps_unavailable".into()),
            timing_uncertainty_us: measured.map(|m| m.uncertainty.as_micros() as u64),
            first_retained_sample_us: measured.map(|m| relative_us(m.first, t0)),
            retained_through_us: measured.map(|m| relative_us(m.through, t0)),
            first_delivery_us: capture.first_delivery.map(|t| relative_us(t, t0)),
            first_vad_output_us: capture.first_vad.map(|t| relative_us(t, t0)),
            vad_source_range: capture.first_vad_range,
            valid_frames: capture.valid_frames,
            dropped_frames: capture.dropped_frames,
            discontinuities: measured.map(|m| m.discontinuities),
            capture_failed: capture.capture_failed,
            speech_detected: capture.speech_detected,
        };
        let output = output.map(|o| OutputRecord {
            completion_basis: if let OutputResult::Completed(basis) = o.result {
                Some(basis)
            } else {
                None
            },
            kind: o.kind,
            result: o.result,
            settled_us: relative_us(o.at, t0),
        });
        let metrics = derive_metrics(
            &capture,
            self.stop.map(|(t, _)| relative_us(t, t0)),
            output.as_ref(),
            outcome,
        );
        let dropped = self.diagnostics.0.dropped.load(Ordering::Relaxed);
        let mut phases = std::collections::HashSet::new();
        for event in &events {
            match event.event {
                Event::PhaseStarted { phase_id, .. } => {
                    phases.insert(phase_id);
                }
                Event::PhaseCompleted { phase_id, .. } => {
                    phases.remove(&phase_id);
                }
                _ => {}
            }
        }
        let events_complete = dropped == 0 && phases.is_empty();
        let requested = self.config.get("post_process").is_some_and(|v| v == "true");
        let mut attempts = Vec::new();
        let mut actual_attempts = 0;
        let mut last_outcome = None;
        let mut skipped = None;
        for event in &events {
            match event.event {
                Event::PhaseStarted {
                    kind: PhaseKind::PostProcessAttempt,
                    ..
                } => {
                    actual_attempts += 1;
                }
                Event::PhaseStarted {
                    phase_id,
                    kind: PhaseKind::PostProcess,
                } => attempts.push(phase_id),
                Event::PhaseCompleted { phase_id, outcome } if attempts.contains(&phase_id) => {
                    last_outcome = Some(outcome)
                }
                Event::PostProcessSkipped { reason } => skipped = Some(reason),
                _ => {}
            }
        }
        let post_process = PostProcessRecord {
            requested,
            attempted: if actual_attempts > 0 {
                Some(true)
            } else if events_complete || skipped.is_some() || !requested {
                Some(false)
            } else {
                None
            },
            attempts: actual_attempts,
            outcome: if !requested {
                "not_requested"
            } else if skipped.is_some() {
                "skipped"
            } else {
                match last_outcome {
                    Some(PhaseOutcome::Success) => "success",
                    Some(PhaseOutcome::Failed) => "failed_original_text_fallback",
                    Some(PhaseOutcome::Cancelled) => "cancelled",
                    _ => "incomplete",
                }
            }
            .into(),
            skipped_reason: skipped,
        };
        let mut metadata_truncated = false;
        let config = self
            .config
            .iter()
            .filter_map(|(key, value)| {
                if !CONFIG_FIELDS.contains(&key.as_str()) {
                    metadata_truncated = true;
                    return None;
                }
                let (value, truncated) = bounded_identifier(value);
                metadata_truncated |= truncated;
                Some((key.clone(), value))
            })
            .collect();
        let language = self.language.as_ref().map(|value| {
            let (value, truncated) = bounded_identifier(value);
            metadata_truncated |= truncated;
            value
        });
        FinalizedTrace {
            schema_version: 1,
            run_id: self.activation.run_id.clone(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            started_at_utc: self.activation.utc.clone(),
            origin: self.activation.origin,
            config,
            language,
            outcome,
            capture,
            output,
            post_process,
            metadata_truncated,
            events,
            metrics,
            events_complete,
            dropped_events: dropped,
        }
    }
}
impl Drop for RunOwner {
    fn drop(&mut self) {
        if !self.finished {
            let trace = self.finalize(TerminalOutcome::UnfinishedOwner, Instant::now(), None);
            self.stats.record_trace(&trace, self.generation);
        }
        self.stats.owner_finished();
    }
}
pub fn relative_us(at: Instant, origin: Instant) -> i64 {
    if at >= origin {
        i64::try_from(at.duration_since(origin).as_micros()).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(origin.duration_since(at).as_micros()).unwrap_or(i64::MAX)
    }
}
pub fn derive_metrics(
    capture: &CaptureRecord,
    stop: Option<i64>,
    output: Option<&OutputRecord>,
    outcome: TerminalOutcome,
) -> Metrics {
    let capture_gap = if capture.dropped_frames > 0 {
        Metric::unavailable("capture_discontinuous")
    } else {
        match (
            capture.first_retained_sample_us,
            capture.retained_through_us,
            capture.discontinuities,
        ) {
            (_, _, Some(n)) if n > 0 => Metric::unavailable("capture_discontinuous"),
            (Some(first), Some(through), Some(0))
                if through >= 0 && through >= first && capture.valid_frames > 0 =>
            {
                Metric::measured(first.max(0), capture.timing_uncertainty_us.unwrap_or(0))
            }
            (Some(_), Some(_), _) => Metric::unavailable("coverage_does_not_reach_activation"),
            _ => Metric::unavailable(
                capture
                    .reason
                    .as_deref()
                    .unwrap_or("capture_continuity_unknown"),
            ),
        }
    };
    let output_gap = match (stop, output) {
        (Some(stop), _) if stop < 0 => Metric::unavailable("stop_before_activation"),
        (Some(stop), Some(o))
            if outcome == TerminalOutcome::OutputCompleted
                && matches!(o.result, OutputResult::Completed(_))
                && o.settled_us >= stop =>
        {
            Metric::measured(o.settled_us - stop, 0)
        }
        (None, _) => Metric::unavailable("stop_input_unavailable"),
        (_, Some(o)) if o.settled_us < stop.unwrap_or(0) => {
            Metric::unavailable("output_before_stop")
        }
        _ => Metric::unavailable("output_not_completed"),
    };
    Metrics {
        capture_gap,
        output_gap,
    }
}
