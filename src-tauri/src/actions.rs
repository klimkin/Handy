#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::VadPolicy;
use crate::managers::audio::{recording_error_type, AudioRecordingManager};
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::stats::trace::{
    CompletionBasis, Event, OutputFailure, OutputKind, OutputResult, OutputSettlement, PhaseKind,
    PhaseOutcome, RunOwner, TerminalOutcome, TraceReporter,
};
use crate::managers::stats::{requested_config, run_language};
use crate::managers::transcription::policy::{live_decode, LiveDecode, ModelTraits};
use crate::managers::transcription::StreamWorkKind;
use crate::managers::transcription::{Transcription, TranscriptionManager};
use crate::settings::{get_settings, AppSettings, OverlayStyle, APPLE_INTELLIGENCE_PROVIDER_ID};
use crate::shortcut;
use crate::tray::{set_tray_state, TrayIconState};
use crate::utils::{
    self, show_processing_overlay, show_recording_overlay, show_transcribing_overlay,
};
use crate::TranscriptionCoordinator;
use log::{debug, error, warn};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Manager;
use tauri::{AppHandle, Emitter};
use tauri_specta::Event as _;

/// How a dictation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionOutcome {
    /// The text was typed.
    Typed,
    /// Nothing was heard, so nothing was typed.
    Empty,
    /// The microphone sent only digital silence (e.g. muted).
    Silent,
    /// Transcribing or pasting failed.
    Failed,
}

/// Emitted when a dictation finishes (not on cancel); onboarding uses it to measure speed.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct TranscriptionCompleteEvent {
    pub outcome: TranscriptionOutcome,
    pub audio_seconds: f64,
    pub wait_seconds: f64,
}

fn finish_dictation(
    app: &AppHandle,
    outcome: TranscriptionOutcome,
    sample_count: usize,
    stop_time: Instant,
    stats: Option<RunOwner>,
) {
    finish_dictation_at(app, outcome, sample_count, stop_time, stats, Instant::now());
}
fn finish_dictation_at(
    app: &AppHandle,
    outcome: TranscriptionOutcome,
    sample_count: usize,
    stop_time: Instant,
    stats: Option<RunOwner>,
    completed_at: Instant,
) {
    let wait_seconds = completed_at
        .saturating_duration_since(stop_time)
        .as_secs_f64();
    if let Some(stats) = stats {
        let terminal = match outcome {
            TranscriptionOutcome::Typed => TerminalOutcome::OutputCompleted,
            TranscriptionOutcome::Empty | TranscriptionOutcome::Silent => TerminalOutcome::Empty,
            TranscriptionOutcome::Failed => TerminalOutcome::ProcessingFailed,
        };
        stats.finish(terminal, completed_at, None);
    }
    let event = TranscriptionCompleteEvent {
        outcome,
        audio_seconds: sample_count as f64
            / crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE as f64,
        wait_seconds,
    };
    if let Err(e) = event.emit(app) {
        warn!("Failed to emit transcription complete: {e}");
    }
}

/// Nothing to type: `Silent` if the microphone sent only zeros, else `Empty`.
fn empty_outcome(input_silent: bool) -> TranscriptionOutcome {
    if input_silent {
        TranscriptionOutcome::Silent
    } else {
        TranscriptionOutcome::Empty
    }
}

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bumped per recording session, so a late paste from an earlier session
/// doesn't reset the tray or hide the overlay of a newer one.
static RECORDING_SESSION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, serde::Serialize)]
struct RecordingErrorEvent {
    error_type: String,
    detail: Option<String>,
}

/// Drop guard that finishes the transcription pipeline, including immediate
/// model unloading on early exits.
struct FinishGuard(AppHandle, Arc<TranscriptionManager>);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.1.maybe_unload_immediately("transcription session");
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
        // The pipeline just freed its large transient buffers (captured PCM,
        // WAV copy, engine scratch); hand the cached pages back to the OS so
        // they don't sit in malloc arenas until they get swapped out (#1792).
        crate::memory::trim_freed_memory();
    }
}

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start_traced(
        &self,
        app: &AppHandle,
        binding_id: &str,
        shortcut_str: &str,
        triggered_at: Instant,
        _trace: TraceReporter,
    ) -> Option<TerminalOutcome> {
        self.start(app, binding_id, shortcut_str, triggered_at);
        None
    }
    fn stop_traced(
        &self,
        app: &AppHandle,
        binding_id: &str,
        shortcut_str: &str,
        triggered_at: Instant,
        _run: RunOwner,
    ) {
        self.stop(app, binding_id, shortcut_str, triggered_at);
    }
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str, triggered_at: Instant);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str, triggered_at: Instant);
}

// Transcribe Action
struct TranscribeAction {
    post_process: bool,
}

/// Field name for structured output JSON schema
const TRANSCRIPTION_FIELD: &str = "transcription";

/// Strip invisible Unicode characters that some LLMs may insert
fn strip_invisible_chars(s: &str) -> String {
    s.replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "")
}

/// Strip a leading `<think>...</think>` block. Some endpoints can't disable
/// reasoning, and some local servers put the reasoning text into `content`
/// instead of a separate field — without this the user would get the model's
/// chain of thought pasted along with the cleaned transcription.
fn strip_think_block(s: &str) -> &str {
    if let Some(rest) = s.trim_start().strip_prefix("<think>") {
        if let Some(end) = rest.find("</think>") {
            return rest[end + "</think>".len()..].trim_start();
        }
    }
    s
}

/// Build a system prompt from the user's prompt template.
/// Removes `${output}` placeholder since the transcription is sent as the user message.
fn build_system_prompt(prompt_template: &str) -> String {
    prompt_template.replace("${output}", "").trim().to_string()
}

/// Returns `true` when a transcription has no meaningful content to
/// post-process (empty or whitespace-only). Used to skip the post-processing
/// LLM call when nothing was actually transcribed, which would otherwise make
/// the model reply with an error message such as "you need to provide the
/// transcription".
fn is_blank_transcription(transcription: &str) -> bool {
    transcription.trim().is_empty()
}

async fn complete_unless_cancelled<F, C>(operation: F, is_cancelled: C) -> Option<F::Output>
where
    F: Future,
    C: Fn() -> bool,
{
    tokio::pin!(operation);

    loop {
        if is_cancelled() {
            return None;
        }

        if let Ok(result) =
            tokio::time::timeout(CANCELLATION_POLL_INTERVAL, operation.as_mut()).await
        {
            return Some(result);
        }
    }
}

fn should_use_streaming_overlay(style: OverlayStyle, is_streaming: bool) -> bool {
    style == OverlayStyle::Live && is_streaming
}

struct PostProcessAttempt {
    text: Option<String>,
    #[cfg(test)]
    elapsed: Duration,
}

#[cfg(test)]
async fn post_process_transcription(
    settings: &AppSettings,
    transcription: &str,
) -> Option<PostProcessAttempt> {
    post_process_transcription_traced(settings, transcription, None).await
}
async fn post_process_transcription_traced(
    settings: &AppSettings,
    transcription: &str,
    trace: Option<TraceReporter>,
) -> Option<PostProcessAttempt> {
    if is_blank_transcription(transcription) {
        debug!("Post-processing skipped because the transcription is empty");
        if let Some(t) = &trace {
            t.event(
                Instant::now(),
                Event::PostProcessSkipped {
                    reason: crate::managers::stats::trace::SkipReason::EmptyInput,
                },
            );
        }
        return None;
    }

    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => {
            debug!("Post-processing enabled but no provider is selected");
            if let Some(t) = &trace {
                t.event(
                    Instant::now(),
                    Event::PostProcessSkipped {
                        reason: crate::managers::stats::trace::SkipReason::ProviderUnavailable,
                    },
                );
            }
            return None;
        }
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        debug!(
            "Post-processing skipped because provider '{}' has no model configured",
            provider.id
        );
        if let Some(t) = &trace {
            t.event(
                Instant::now(),
                Event::PostProcessSkipped {
                    reason: crate::managers::stats::trace::SkipReason::ModelUnavailable,
                },
            );
        }
        return None;
    }

    let selected_prompt_id = match &settings.post_process_selected_prompt_id {
        Some(id) => id.clone(),
        None => {
            debug!("Post-processing skipped because no prompt is selected");
            if let Some(t) = &trace {
                t.event(
                    Instant::now(),
                    Event::PostProcessSkipped {
                        reason: crate::managers::stats::trace::SkipReason::PromptUnavailable,
                    },
                );
            }
            return None;
        }
    };

    let prompt = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == selected_prompt_id)
    {
        Some(prompt) => prompt.prompt.clone(),
        None => {
            debug!(
                "Post-processing skipped because prompt '{}' was not found",
                selected_prompt_id
            );
            if let Some(t) = &trace {
                t.event(
                    Instant::now(),
                    Event::PostProcessSkipped {
                        reason: crate::managers::stats::trace::SkipReason::PromptUnavailable,
                    },
                );
            }
            return None;
        }
    };

    if prompt.trim().is_empty() {
        debug!("Post-processing skipped because the selected prompt is empty");
        if let Some(t) = &trace {
            t.event(
                Instant::now(),
                Event::PostProcessSkipped {
                    reason: crate::managers::stats::trace::SkipReason::PromptUnavailable,
                },
            );
        }
        return None;
    }

    debug!(
        "Starting LLM post-processing with provider '{}' (model: {})",
        provider.id, model
    );

    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    // Ask these providers to skip reasoning/thinking — post-processing rarely
    // benefits from it and it adds seconds of latency. llm_client picks the
    // field the endpoint understands and retries without it if rejected.
    let disable_reasoning = matches!(provider.id.as_str(), "custom" | "openrouter");

    if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        if !apple_intelligence::check_apple_intelligence_availability() {
            debug!("Apple Intelligence selected but not currently available on this device");
            if let Some(t) = &trace {
                t.event(
                    Instant::now(),
                    Event::PostProcessSkipped {
                        reason: crate::managers::stats::trace::SkipReason::ProviderUnavailable,
                    },
                );
            }
            return None;
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            debug!("Apple Intelligence provider selected on unsupported platform");
            if let Some(t) = &trace {
                t.event(
                    Instant::now(),
                    Event::PostProcessSkipped {
                        reason: crate::managers::stats::trace::SkipReason::ProviderUnavailable,
                    },
                );
            }
            return None;
        }
    }

    #[cfg(test)]
    let started = Instant::now();
    let mut phase = trace.as_ref().map(|t| t.begin(PhaseKind::PostProcess));
    let text = async {
        if provider.supports_structured_output {
            debug!("Using structured outputs for provider '{}'", provider.id);

            let system_prompt = build_system_prompt(&prompt);
            let user_content = transcription.to_string();

            // Handle Apple Intelligence separately since it uses native Swift APIs
            if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                {
                    let token_limit = model.trim().parse::<i32>().unwrap_or(0);
                    let attempt = trace.as_ref().map(|t| t.begin(PhaseKind::PostProcessAttempt));
                    let result = apple_intelligence::process_text_with_system_prompt(
                        &system_prompt,
                        &user_content,
                        token_limit,
                    );
                    if let (Some(t), Some(id)) = (&trace, attempt) {
                        t.end(
                            id,
                            if result.is_ok() {
                                PhaseOutcome::Success
                            } else {
                                PhaseOutcome::Failed
                            },
                        );
                    }
                    return match result {
                        Ok(result) => {
                            if result.trim().is_empty() {
                                debug!("Apple Intelligence returned an empty response");
                                None
                            } else {
                                let result = strip_invisible_chars(&result);
                                debug!(
                                    "Apple Intelligence post-processing succeeded. Output length: {} chars",
                                    result.len()
                                );
                                Some(result)
                            }
                        }
                        Err(err) => {
                            error!("Apple Intelligence post-processing failed: {}", err);
                            None
                        }
                    };
                }

            }

            // Define JSON schema for transcription output
            let json_schema = serde_json::json!({
                "type": "object",
                "properties": {
                    (TRANSCRIPTION_FIELD): {
                        "type": "string",
                        "description": "The cleaned and processed transcription text"
                    }
                },
                "required": [TRANSCRIPTION_FIELD],
                "additionalProperties": false
            });

            match crate::llm_client::send_chat_completion_with_schema(
                &provider,
                api_key.clone(),
                &model,
                user_content,
                Some(system_prompt),
                Some(json_schema),
                crate::llm_client::CompletionOptions{disable_reasoning,trace:trace.clone()},
            )
            .await
            {
                Ok(Some(content)) => {
                    // Parse the JSON response to extract the transcription field
                    let content = strip_think_block(&content);
                    match serde_json::from_str::<serde_json::Value>(content) {
                        Ok(json) => {
                            if let Some(transcription_value) =
                                json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str())
                            {
                                let result = strip_invisible_chars(transcription_value);
                                debug!(
                                    "Structured output post-processing succeeded for provider '{}'. Output length: {} chars",
                                    provider.id,
                                    result.len()
                                );
                                return Some(result);
                            } else {
                                error!("Structured output response missing 'transcription' field");
                                return Some(strip_invisible_chars(content));
                            }
                        }
                        Err(e) => {
                            error!(
                                "Failed to parse structured output JSON: {}. Returning raw content.",
                                e
                            );
                            return Some(strip_invisible_chars(content));
                        }
                    }
                }
                Ok(None) => {
                    error!("LLM API response has no content");
                    return None;
                }
                Err(e) => {
                    warn!(
                        "Structured output failed for provider '{}': {}. Falling back to legacy mode.",
                        provider.id, e
                    );
                    if let (Some(t),Some(id))=(&trace,phase){t.end(id,PhaseOutcome::Failed);t.event(Instant::now(),Event::PostProcessFallback);}
                    phase=trace.as_ref().map(|t|t.begin(PhaseKind::PostProcess));
                    // Fall through to legacy mode below
                }
            }
        }

        // Legacy mode: Replace ${output} variable in the prompt with the actual text
        let processed_prompt = prompt.replace("${output}", transcription);
        debug!("Processed prompt length: {} chars", processed_prompt.len());

        match crate::llm_client::send_chat_completion(
            &provider,
            api_key,
            &model,
            processed_prompt,
            crate::llm_client::CompletionOptions{disable_reasoning,trace:trace.clone()},
        )
        .await
        {
            Ok(Some(content)) => {
                let content = strip_invisible_chars(strip_think_block(&content));
                debug!(
                    "LLM post-processing succeeded for provider '{}'. Output length: {} chars",
                    provider.id,
                    content.len()
                );
                Some(content)
            }
            Ok(None) => {
                error!("LLM API response has no content");
                None
            }
            Err(e) => {
                error!(
                    "LLM post-processing failed for provider '{}': {}. Falling back to original transcription.",
                    provider.id,
                    e
                );
                None
            }
        }
    }
    .await;
    if let (Some(t), Some(id)) = (&trace, phase) {
        t.end(
            id,
            if text.is_some() {
                PhaseOutcome::Success
            } else {
                PhaseOutcome::Failed
            },
        );
    }
    Some(PostProcessAttempt {
        text,
        #[cfg(test)]
        elapsed: started.elapsed(),
    })
}

pub(crate) struct ProcessedTranscription {
    pub final_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
}

pub(crate) async fn process_transcription_output(
    app: &AppHandle,
    transcription: &str,
    post_process: bool,
) -> ProcessedTranscription {
    let settings = get_settings(app);
    process_transcription_output_traced(&settings, transcription, post_process, None).await
}
async fn process_transcription_output_traced(
    settings: &AppSettings,
    transcription: &str,
    post_process: bool,
    trace: Option<TraceReporter>,
) -> ProcessedTranscription {
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;

    if post_process {
        if let Some(attempt) =
            post_process_transcription_traced(settings, &final_text, trace.clone()).await
        {
            if let Some(processed_text) = attempt.text {
                post_processed_text = Some(processed_text.clone());
                final_text = processed_text;

                if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                    if let Some(prompt) = settings
                        .post_process_prompts
                        .iter()
                        .find(|prompt| &prompt.id == prompt_id)
                    {
                        post_process_prompt = Some(prompt.prompt.clone());
                    }
                }
            }
        }
    }

    if !post_process {
        if let Some(t) = trace {
            t.event(
                Instant::now(),
                Event::PostProcessSkipped {
                    reason: crate::managers::stats::trace::SkipReason::NotRequested,
                },
            );
        }
    }
    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
    }
}

fn refresh_stage_config(run: &mut RunOwner, settings: &AppSettings, requested: bool, llm: bool) {
    let fresh = requested_config(settings, &settings.selected_model, requested);
    if llm {
        run.config.retain(|key, _| !key.starts_with("post_process"));
    }
    for (key, value) in fresh {
        if (llm && key.starts_with("post_process"))
            || (!llm
                && matches!(
                    key.as_str(),
                    "paste_method"
                        | "paste_delay_ms"
                        | "paste_delay_after_ms"
                        | "auto_submit"
                        | "auto_submit_key"
                        | "clipboard_handling"
                        | "reliable_paste"
                        | "append_trailing_space"
                        | "typing_tool"
                ))
        {
            run.config.insert(key, value);
        }
    }
}

impl ShortcutAction for TranscribeAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str, triggered_at: Instant) {
        let _ = self.start_impl(app, binding_id, shortcut_str, triggered_at, None);
    }
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str, triggered_at: Instant) {
        self.stop_impl(app, binding_id, shortcut_str, triggered_at, None);
    }
    fn start_traced(
        &self,
        app: &AppHandle,
        binding_id: &str,
        shortcut_str: &str,
        triggered_at: Instant,
        trace: TraceReporter,
    ) -> Option<TerminalOutcome> {
        self.start_impl(app, binding_id, shortcut_str, triggered_at, Some(trace))
    }
    fn stop_traced(
        &self,
        app: &AppHandle,
        binding_id: &str,
        shortcut_str: &str,
        triggered_at: Instant,
        run: RunOwner,
    ) {
        self.stop_impl(app, binding_id, shortcut_str, triggered_at, Some(run));
    }
}
impl TranscribeAction {
    fn start_impl(
        &self,
        app: &AppHandle,
        binding_id: &str,
        _shortcut_str: &str,
        _triggered_at: Instant,
        trace: Option<TraceReporter>,
    ) -> Option<TerminalOutcome> {
        let start_time = Instant::now();
        debug!("TranscribeAction::start called for binding: {}", binding_id);

        // Load model in the background
        let tm = app.state::<Arc<TranscriptionManager>>();
        let rm = app.state::<Arc<AudioRecordingManager>>();

        // Load ASR model and VAD model in parallel
        let kickoff_started = Instant::now();
        tm.initiate_model_load_traced(trace.clone());
        let rm_clone = Arc::clone(&rm);
        let vad_trace = trace.clone();
        std::thread::spawn(move || {
            let result = rm_clone.preload_vad_traced(vad_trace.as_ref());
            if let Err(e) = result {
                debug!("VAD pre-load failed: {}", e);
            }
        });
        let kickoff_elapsed = kickoff_started.elapsed();

        // Don't open the mic if nothing can transcribe the recording; the load
        // kicked off above fails and reports why.
        if !tm.is_model_loaded() {
            let selected_model = get_settings(app).selected_model;
            if let Err(e) = app
                .state::<Arc<ModelManager>>()
                .get_model_path(&selected_model)
            {
                warn!("Not starting recording: no model can transcribe it ({})", e);
                return Some(TerminalOutcome::ProcessingFailed);
            }
        }

        let binding_id = binding_id.to_string();
        RECORDING_SESSION.fetch_add(1, Ordering::SeqCst);
        let tray_started = Instant::now();
        set_tray_state(app, TrayIconState::Recording);
        let tray_elapsed = tray_started.elapsed();

        // Get the microphone mode to determine audio feedback timing
        let plan_started = Instant::now();
        let settings = get_settings(app);
        let is_always_on = settings.always_on_microphone;
        let selected_model_info = app
            .state::<Arc<ModelManager>>()
            .get_model_info(&settings.selected_model);

        // Use the app-facing model capability as the single pre-recording source
        // for live streaming decisions. Unknown support is represented as false
        // until the model registry is updated by discovery or runtime load.
        let model_supports_streaming = selected_model_info
            .as_ref()
            .map(|m| m.supports_streaming)
            .unwrap_or(false);
        let vad_policy = if !settings.vad_enabled {
            VadPolicy::Disabled
        } else if model_supports_streaming {
            VadPolicy::Streaming
        } else {
            VadPolicy::Offline
        };
        // Before the model loads, whether it keeps its accuracy on long audio
        // is unknown, so assume not: the worker decides again with the loaded
        // model, and a wrong guess costs only an idle worker.
        let expected_model = ModelTraits {
            streaming: model_supports_streaming,
            long_form: false,
        };
        if live_decode(expected_model, settings.long_form_chunking) != LiveDecode::None {
            tm.start_stream_traced(trace.clone());
        }
        let plan_elapsed = plan_started.elapsed();

        // Sizing the overlay follows the same advertised capability. A model that
        // doesn't stream (or whose capability is not known yet) gets the compact
        // pill instead of an oversized transparent live window.
        let overlay_started = Instant::now();
        match settings.overlay_style {
            OverlayStyle::Live if model_supports_streaming => utils::show_streaming_overlay(app),
            OverlayStyle::Live | OverlayStyle::Minimal => show_recording_overlay(app),
            OverlayStyle::None => {} // show_overlay_state no-ops on None anyway
        }
        // Everything above runs before capture can begin, so each span here is
        // added keypress->capture latency.
        debug!(
            "start-path pre-recording steps: model_kickoff={:?} tray={:?} settings+stream_plan={:?} overlay={:?}",
            kickoff_elapsed,
            tray_elapsed,
            plan_elapsed,
            overlay_started.elapsed()
        );
        debug!("Microphone mode - always_on: {}", is_always_on);

        let mut recording_error: Option<anyhow::Error> = None;
        let recording_start_time = Instant::now();
        if let Some(t) = &trace {
            t.event(recording_start_time, Event::RecordingRequested);
        }
        match rm.try_start_recording(&binding_id, vad_policy, trace.as_ref()) {
            Ok(readiness) => {
                debug!(
                    "Recording request accepted in {:?}; waiting for first microphone samples",
                    recording_start_time.elapsed()
                );
                let generation = readiness.generation();
                let app_clone = app.clone();
                let rm_clone = Arc::clone(&rm);
                std::thread::spawn(move || {
                    if !readiness.wait() {
                        debug!("Microphone readiness wait ended without receiving samples");
                        return;
                    }

                    // Development-only preview hook for evaluating the brief
                    // arming animation on hardware that normally starts too fast
                    // to make it visible.
                    #[cfg(debug_assertions)]
                    if let Ok(delay_ms) = std::env::var("HANDY_DEBUG_MIC_READY_DELAY_MS")
                        .unwrap_or_default()
                        .parse::<u64>()
                    {
                        let delay_ms = delay_ms.min(10_000);
                        if delay_ms > 0 {
                            debug!("Delaying microphone-ready cue by {delay_ms}ms for UI preview");
                            std::thread::sleep(Duration::from_millis(delay_ms));
                        }
                    }

                    if !rm_clone.is_recording_readiness_current(generation) {
                        debug!("Microphone became ready for an inactive recording");
                        return;
                    }

                    debug!("Microphone is receiving samples; recording is ready");
                    utils::emit_recording_ready(&app_clone);

                    // The start chime is a readiness cue, so it must follow the
                    // first real input callback rather than Stream::play() or a
                    // fixed delay. The helper returns immediately when feedback
                    // is disabled; mute still follows the same readiness point.
                    if rm_clone.is_recording_readiness_current(generation) {
                        play_feedback_sound_blocking(&app_clone, SoundType::Start);
                    }
                    if rm_clone.is_recording_readiness_current(generation) {
                        rm_clone.apply_mute();
                    }
                });
            }
            Err(e) => {
                debug!("Failed to start recording: {}", e);
                recording_error = Some(e);
            }
        }

        let startup_failed = recording_error.is_some();
        if recording_error.is_none() {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);
        } else {
            // Starting failed (for example due to blocked microphone permissions).
            // Revert UI state so we don't stay stuck in the recording overlay.
            tm.cancel_stream();
            utils::hide_recording_overlay(app);
            set_tray_state(app, TrayIconState::Idle);
            if let Some(err) = recording_error {
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: recording_error_type(&err).to_string(),
                        detail: Some(format!("{err:#}")),
                    },
                );
            }
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
        startup_failed.then_some(TerminalOutcome::CaptureFailed)
    }

    fn stop_impl(
        &self,
        app: &AppHandle,
        binding_id: &str,
        _shortcut_str: &str,
        triggered_at: Instant,
        mut stats: Option<RunOwner>,
    ) {
        if let Some(run) = stats.as_mut() {
            run.set_stop(triggered_at);
        }
        let trace = stats.as_ref().map(RunOwner::reporter);
        // Prevent a slow microphone from emitting a ready event or start chime
        // after the user has already requested stop.
        app.state::<Arc<AudioRecordingManager>>()
            .invalidate_recording_readiness();

        // Unregister the cancel shortcut when transcription stops
        shortcut::unregister_cancel_shortcut(app);

        let stop_time = triggered_at;
        debug!("TranscribeAction::stop called for binding: {}", binding_id);

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

        set_tray_state(app, TrayIconState::Transcribing);
        // Stop should give immediate visual feedback. Live streaming can keep
        // the larger panel, but it still switches from listening to a working
        // spinner while the stream finalizes. Non-streaming paths use the
        // compact transcribing pill (None no-ops in show_*).
        let style = get_settings(app).overlay_style;
        // Capture this before finalizing the stream so every later working state
        // targets the same overlay that was shown for this transcription.
        let use_streaming_overlay = should_use_streaming_overlay(style, tm.is_streaming());
        if use_streaming_overlay {
            tm.emit_stream_working(StreamWorkKind::Transcribing);
        } else {
            show_transcribing_overlay(app);
        }

        // Unmute before playing audio feedback so the stop sound is audible
        rm.remove_mute();

        // Play audio feedback for recording stop
        play_feedback_sound(app, SoundType::Stop);

        let binding_id = binding_id.to_string(); // Clone binding_id for the async task
        let post_process = self.post_process;
        let cancel_generation = rm.cancel_generation();

        tauri::async_runtime::spawn(async move {
            let _guard = FinishGuard(ah.clone(), Arc::clone(&tm));
            debug!(
                "Starting async transcription task for binding: {}",
                binding_id
            );

            let stop_recording_time = Instant::now();
            if let Some(recording) = rm.stop_recording(&binding_id, cancel_generation) {
                let capture_failed = recording.capture.capture_failed;
                if let Some(run) = stats.as_mut() {
                    run.captured(recording.capture);
                }
                let input_silent = recording.input_silent;
                let samples = recording.samples;
                debug!(
                    "Recording stopped and samples retrieved in {:?}, sample count: {}",
                    stop_recording_time.elapsed(),
                    samples.len()
                );

                if rm.was_cancelled_since(cancel_generation) {
                    debug!("Transcription operation cancelled after recording stop");
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                    if let Some(run) = stats.take() {
                        run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
                    }
                    return;
                }

                if samples.is_empty() {
                    debug!("Recording produced no audio samples; retaining capture diagnostics");
                    // Tear down any streaming worker so its channel doesn't leak
                    // and block the next start_stream.
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                    if capture_failed {
                        if let Some(run) = stats.take() {
                            run.finish(TerminalOutcome::CaptureFailed, Instant::now(), None);
                        }
                    }
                    finish_dictation(&ah, empty_outcome(input_silent), 0, stop_time, stats);
                } else {
                    // Save WAV concurrently with transcription
                    let sample_count = samples.len();
                    let file_name = format!("handy-{}.wav", chrono::Utc::now().timestamp());
                    let wav_path = hm.recordings_dir().join(&file_name);
                    let wav_path_for_verify = wav_path.clone();
                    let samples_for_wav = samples.clone();
                    let wav_trace = trace.clone();
                    let wav_handle = tauri::async_runtime::spawn_blocking(move || {
                        let phase = wav_trace.as_ref().map(|t| t.begin(PhaseKind::WavWrite));
                        let result =
                            crate::audio_toolkit::save_wav_file(&wav_path, &samples_for_wav);
                        if let (Some(t), Some(id)) = (&wav_trace, phase) {
                            t.end(
                                id,
                                if result.is_ok() {
                                    PhaseOutcome::Success
                                } else {
                                    PhaseOutcome::Failed
                                },
                            );
                        }
                        result
                    });

                    // Transcribe concurrently with WAV save: the live decode's
                    // result when it has one (all audio was already fed to it),
                    // otherwise a batch run of the samples. A cancel is surfaced
                    // as an error, so it never starts a batch run.
                    let transcription_time = Instant::now();
                    let transcription_result =
                        tm.transcribe_recording_traced(samples, trace.clone());

                    // Await WAV save and verify
                    let wav_saved = match wav_handle.await {
                        Ok(Ok(())) => {
                            let phase = trace.as_ref().map(|t| t.begin(PhaseKind::WavVerify));
                            let result = crate::audio_toolkit::verify_wav_file(
                                &wav_path_for_verify,
                                sample_count,
                            );
                            if let (Some(t), Some(id)) = (&trace, phase) {
                                t.end(
                                    id,
                                    if result.is_ok() {
                                        PhaseOutcome::Success
                                    } else {
                                        PhaseOutcome::Failed
                                    },
                                );
                            }
                            match result {
                                Ok(()) => true,
                                Err(e) => {
                                    error!("WAV verification failed: {}", e);
                                    false
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            error!("Failed to save WAV file: {}", e);
                            false
                        }
                        Err(e) => {
                            error!("WAV save task panicked: {}", e);
                            false
                        }
                    };

                    if rm.was_cancelled_since(cancel_generation) {
                        debug!("Transcription operation cancelled before output handling");
                        utils::hide_recording_overlay(&ah);
                        set_tray_state(&ah, TrayIconState::Idle);
                        if let Some(run) = stats.take() {
                            run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
                        }
                        return;
                    }

                    match transcription_result {
                        Ok(Transcription {
                            text: transcription,
                            language,
                            model_id,
                        }) => {
                            debug!(
                                "Transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                utils::redact_text(&transcription)
                            );
                            if let Some(stats) = stats.as_mut() {
                                if let Some(model_id) = model_id {
                                    stats.config.insert("model".into(), model_id);
                                }
                                stats.language = run_language(
                                    language.as_ref(),
                                    &stats.config["selected_language"],
                                );
                            }

                            if post_process {
                                if use_streaming_overlay {
                                    tm.emit_stream_working(StreamWorkKind::Polishing);
                                } else {
                                    show_processing_overlay(&ah);
                                }
                            }
                            let processing_settings = get_settings(&ah);
                            if let Some(run) = stats.as_mut() {
                                refresh_stage_config(run, &processing_settings, post_process, true);
                            }
                            let Some(processed) = complete_unless_cancelled(
                                process_transcription_output_traced(
                                    &processing_settings,
                                    &transcription,
                                    post_process,
                                    trace.clone(),
                                ),
                                || rm.was_cancelled_since(cancel_generation),
                            )
                            .await
                            else {
                                debug!("Transcription operation cancelled during output handling");
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                if let Some(run) = stats.take() {
                                    run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
                                }
                                return;
                            };

                            if rm.was_cancelled_since(cancel_generation) {
                                debug!("Transcription operation cancelled before paste");
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                if let Some(run) = stats.take() {
                                    run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
                                }
                                return;
                            }

                            // Save to history if WAV was saved
                            if wav_saved {
                                let phase =
                                    trace.as_ref().map(|t| t.begin(PhaseKind::HistoryWrite));
                                let result = hm.save_entry(
                                    file_name,
                                    transcription,
                                    post_process,
                                    processed.post_processed_text.clone(),
                                    processed.post_process_prompt.clone(),
                                    language,
                                );
                                if let (Some(t), Some(id)) = (&trace, phase) {
                                    t.end(
                                        id,
                                        if result.is_ok() {
                                            PhaseOutcome::Success
                                        } else {
                                            PhaseOutcome::Failed
                                        },
                                    );
                                }
                                if let Err(err) = result {
                                    error!("Failed to save history entry: {}", err);
                                }
                            }

                            if processed.final_text.is_empty() {
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                finish_dictation(
                                    &ah,
                                    empty_outcome(input_silent),
                                    sample_count,
                                    stop_time,
                                    stats,
                                );
                            } else {
                                let ah_clone = ah.clone();
                                let paste_time = Instant::now();
                                let final_text = processed.final_text;
                                let rm_for_paste = Arc::clone(&rm);
                                // Taken before the paste thread exists, and so
                                // before the next session can start.
                                let session = RECORDING_SESSION.load(Ordering::SeqCst);
                                let stats_slot = Arc::new(std::sync::Mutex::new(stats.take()));
                                let stats_for_output = Arc::clone(&stats_slot);
                                let trace_for_output = trace.clone();
                                let spawn = std::thread::Builder::new()
                                    .name("paste".into())
                                    .spawn(move || {
                                        let mut run = stats_for_output
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner())
                                            .take();
                                        if rm_for_paste.was_cancelled_since(cancel_generation) {
                                            if let Some(run) = run.take() {
                                                run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
                                            }
                                            return;
                                        }
                                        let settings = get_settings(&ah_clone);
                                        if let Some(run) = run.as_mut() {
                                            refresh_stage_config(run, &settings, post_process, false);
                                        }
                                        let settlement = match trace_for_output {
                                            Some(trace) => crate::clipboard::paste_with_receipt(
                                                final_text,
                                                ah_clone.clone(),
                                                trace,
                                                settings,
                                            ).wait(),
                                            None => {
                                                let result = utils::paste(final_text, ah_clone.clone());
                                                OutputSettlement {
                                                    at: Instant::now(),
                                                    kind: OutputKind::ClipboardPaste,
                                                    result: match result {
                                                        Ok(()) => OutputResult::Completed(CompletionBasis::SynchronousOutputReturn),
                                                        Err(_) => OutputResult::Failed(OutputFailure::Other),
                                                    },
                                                }
                                            }
                                        };
                                        let terminal = match settlement.result {
                                            OutputResult::Completed(_) => TerminalOutcome::OutputCompleted,
                                            OutputResult::Unconfirmed(_) => TerminalOutcome::OutputUnconfirmed,
                                            OutputResult::Failed(_) => TerminalOutcome::OutputFailed,
                                            OutputResult::NotRequested => TerminalOutcome::NoOutputRequested,
                                        };
                                        let outcome = match settlement.result {
                                            OutputResult::Completed(_) | OutputResult::NotRequested => TranscriptionOutcome::Typed,
                                            _ => TranscriptionOutcome::Failed,
                                        };
                                        if let Some(run) = run.take() {
                                            run.finish(terminal, settlement.at, Some(settlement));
                                        }
                                        if outcome == TranscriptionOutcome::Failed {
                                            let _ = ah_clone.emit("paste-error", ());
                                        }
                                        debug!("Output settled after {:?}", paste_time.elapsed());
                                        if RECORDING_SESSION.load(Ordering::SeqCst) == session {
                                            utils::hide_recording_overlay(&ah_clone);
                                            set_tray_state(&ah_clone, TrayIconState::Idle);
                                        }
                                        finish_dictation_at(
                                            &ah_clone, outcome, sample_count, stop_time, None, settlement.at,
                                        );
                                    });
                                if let Err(error) = spawn {
                                    error!("Failed to start paste thread: {error}");
                                    if let Some(run) =
                                        stats_slot.lock().unwrap_or_else(|e| e.into_inner()).take()
                                    {
                                        run.finish(
                                            TerminalOutcome::OutputFailed,
                                            Instant::now(),
                                            None,
                                        );
                                    }
                                    utils::hide_recording_overlay(&ah);
                                    set_tray_state(&ah, TrayIconState::Idle);
                                    finish_dictation(
                                        &ah,
                                        TranscriptionOutcome::Failed,
                                        sample_count,
                                        stop_time,
                                        None,
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            if rm.was_cancelled_since(cancel_generation) {
                                debug!(
                                    "Transcription operation cancelled after transcription error"
                                );
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                if let Some(run) = stats.take() {
                                    run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
                                }
                                return;
                            }

                            error!("Transcription failed: {}", err);
                            // Surface the failure to the UI (toast). The full
                            // message is also in handy.log via the line above.
                            let _ = ah.emit("transcription-error", err.to_string());
                            // Save entry with empty text so user can retry
                            if wav_saved {
                                let phase =
                                    trace.as_ref().map(|t| t.begin(PhaseKind::HistoryWrite));
                                let result = hm.save_entry(
                                    file_name,
                                    String::new(),
                                    post_process,
                                    None,
                                    None,
                                    None,
                                );
                                if let (Some(t), Some(id)) = (&trace, phase) {
                                    t.end(
                                        id,
                                        if result.is_ok() {
                                            PhaseOutcome::Success
                                        } else {
                                            PhaseOutcome::Failed
                                        },
                                    );
                                }
                                if let Err(save_err) = result {
                                    error!("Failed to save failed history entry: {}", save_err);
                                }
                            }
                            utils::hide_recording_overlay(&ah);
                            set_tray_state(&ah, TrayIconState::Idle);
                            finish_dictation(
                                &ah,
                                TranscriptionOutcome::Failed,
                                sample_count,
                                stop_time,
                                stats,
                            );
                        }
                    }
                }
            } else {
                debug!("No samples retrieved from recording stop");
                // Tear down any streaming worker so its channel doesn't leak.
                tm.cancel_stream();
                utils::hide_recording_overlay(&ah);
                set_tray_state(&ah, TrayIconState::Idle);
                if rm.was_cancelled_since(cancel_generation) {
                    if let Some(run) = stats.take() {
                        run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
                    }
                }
                if !rm.was_cancelled_since(cancel_generation) {
                    if let Some(run) = stats.take() {
                        run.finish(TerminalOutcome::CaptureFailed, Instant::now(), None);
                    }
                    finish_dictation(&ah, TranscriptionOutcome::Empty, 0, stop_time, None);
                }
            }
        });

        debug!(
            "TranscribeAction::stop completed in {:?}",
            stop_time.elapsed()
        );
    }
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(
        &self,
        app: &AppHandle,
        _binding_id: &str,
        _shortcut_str: &str,
        triggered_at: Instant,
    ) {
        utils::cancel_current_operation_at(app, triggered_at);
    }

    fn stop(
        &self,
        _app: &AppHandle,
        _binding_id: &str,
        _shortcut_str: &str,
        _triggered_at: Instant,
    ) {
        // Nothing to do on stop for cancel
    }
}

// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str, _triggered_at: Instant) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})", // Changed "Pressed" to "Started" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str, _triggered_at: Instant) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})", // Changed "Released" to "Stopped" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "transcribe".to_string(),
        Arc::new(TranscribeAction {
            post_process: false,
        }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "transcribe_with_post_process".to_string(),
        Arc::new(TranscribeAction { post_process: true }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});

#[cfg(test)]
mod tests {
    use super::{
        complete_unless_cancelled, is_blank_transcription, should_use_streaming_overlay,
        strip_think_block,
    };
    use crate::settings::OverlayStyle;
    use std::future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn post_processing_times_actual_success_and_failed_attempts_not_skips() {
        use crate::settings::{get_default_settings, PostProcessApiFormat, PostProcessProvider};
        let mut settings = get_default_settings();
        assert!(super::post_process_transcription(&settings, "")
            .await
            .is_none());
        settings.post_process_provider_id = "missing".into();
        assert!(super::post_process_transcription(&settings, "hello")
            .await
            .is_none());

        for (success, structured, reasoning) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let provider_id = if reasoning { "custom" } else { "timing-test" };
            settings.post_process_provider_id = provider_id.into();
            settings.post_process_providers = vec![PostProcessProvider {
                id: provider_id.into(),
                label: "Timing test".into(),
                base_url: format!("http://{}", listener.local_addr().unwrap()),
                allow_base_url_edit: true,
                models_endpoint: None,
                supports_structured_output: structured,
                api_format: PostProcessApiFormat::OpenAi,
            }];
            settings
                .post_process_models
                .insert(provider_id.into(), "test".into());
            settings.post_process_selected_prompt_id =
                Some(settings.post_process_prompts[0].id.clone());
            let server = tokio::spawn(async move {
                for request_index in 0..if structured || reasoning { 2 } else { 1 } {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut content_length = 0;
                    loop {
                        let mut header = String::new();
                        assert!(stream.read_line(&mut header).await.unwrap() > 0);
                        if header == "\r\n" {
                            break;
                        }
                        if let Some(length) =
                            header.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            content_length = length.trim().parse::<usize>().unwrap();
                        }
                    }
                    let mut body = vec![0_u8; content_length];
                    stream.read_exact(&mut body).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    let (status, body) =
                        if success && (!(structured || reasoning) || request_index > 0) {
                            (
                                "200 OK",
                                r#"{"choices":[{"message":{"content":"Cleaned"}}]}"#,
                            )
                        } else {
                            ("400 Bad Request", r#"{"error":{"message":"test failure"}}"#)
                        };
                    stream.write_all(format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
                }
            });
            let dir = tempfile::tempdir().unwrap();
            let stats = Arc::new(crate::managers::stats::StatsManager::test_at(
                dir.path().join("stats.db"),
            ));
            let mut run = stats.begin(
                stats.input_stamp(crate::managers::stats::trace::InputOrigin::ExternalCommand),
                crate::managers::stats::requested_config(&settings, "test", true),
            );
            run.set_stop(std::time::Instant::now());
            let attempt =
                super::post_process_transcription_traced(&settings, "hello", Some(run.reporter()))
                    .await
                    .unwrap();
            let done = std::time::Instant::now();
            run.finish(
                crate::managers::stats::trace::TerminalOutcome::OutputCompleted,
                done,
                Some(crate::managers::stats::trace::OutputSettlement {
                    at: done,
                    kind: crate::managers::stats::trace::OutputKind::DirectTyping,
                    result: crate::managers::stats::trace::OutputResult::Completed(
                        crate::managers::stats::trace::CompletionBasis::DirectTypingReturn,
                    ),
                }),
            );
            stats.shutdown().unwrap();
            let json = std::fs::read_to_string(stats.trace_path()).unwrap();
            let trace: crate::managers::stats::trace::FinalizedTrace =
                serde_json::from_str(json.trim()).unwrap();
            assert_eq!(
                trace.post_process.attempts,
                if structured || reasoning { 2 } else { 1 }
            );
            assert_eq!(trace.post_process.attempted, Some(true));
            assert!(trace.metrics.output_gap.us.unwrap() >= attempt.elapsed.as_micros() as i64);
            assert!(!json.contains("127.0.0.1"));
            assert!(!json.contains("hello"));
            server.await.unwrap();
            assert!(
                attempt.elapsed
                    >= Duration::from_millis(if structured || reasoning { 60 } else { 30 })
            );
            assert_eq!(attempt.text.as_deref(), success.then_some("Cleaned"));
        }
    }

    #[tokio::test]
    async fn invalid_api_header_does_not_record_an_llm_attempt() {
        use crate::managers::stats::trace::{FinalizedTrace, InputOrigin, TerminalOutcome};
        use crate::managers::stats::StatsManager;
        use crate::settings::{get_default_settings, PostProcessApiFormat, PostProcessProvider};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = get_default_settings();
        let provider_id = "invalid-header-test";
        settings.post_process_provider_id = provider_id.into();
        settings.post_process_providers = vec![PostProcessProvider {
            id: provider_id.into(),
            label: "Invalid header test".into(),
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            allow_base_url_edit: true,
            models_endpoint: None,
            supports_structured_output: false,
            api_format: PostProcessApiFormat::OpenAi,
        }];
        settings
            .post_process_models
            .insert(provider_id.into(), "test".into());
        settings
            .post_process_api_keys
            .insert(provider_id.into(), "invalid\nheader".into());
        settings.post_process_selected_prompt_id =
            Some(settings.post_process_prompts[0].id.clone());
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(StatsManager::test_at(dir.path().join("stats.db")));
        let run = stats.begin(
            stats.input_stamp(InputOrigin::ExternalCommand),
            crate::managers::stats::requested_config(&settings, "test", true),
        );
        let result =
            super::post_process_transcription_traced(&settings, "hello", Some(run.reporter()))
                .await
                .unwrap();
        assert!(result.text.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
        run.finish(TerminalOutcome::Empty, std::time::Instant::now(), None);
        stats.shutdown().unwrap();
        let json = std::fs::read_to_string(stats.trace_path()).unwrap();
        let trace: FinalizedTrace = serde_json::from_str(json.trim()).unwrap();
        assert_eq!(trace.post_process.attempted, Some(false));
        assert_eq!(trace.post_process.attempts, 0);
        assert_eq!(trace.post_process.outcome, "failed_original_text_fallback");
    }

    #[test]
    fn blank_transcription_is_detected() {
        assert!(is_blank_transcription(""));
        assert!(is_blank_transcription("   "));
        assert!(is_blank_transcription("\t\n  \r\n"));
    }

    #[test]
    fn non_blank_transcription_is_kept() {
        assert!(!is_blank_transcription("hello"));
        assert!(!is_blank_transcription("  hello  "));
    }

    #[test]
    fn completed_operation_returns_its_output() {
        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::ready("done"),
            || false,
        ));

        assert_eq!(result, Some("done"));
    }

    #[test]
    fn pending_operation_stops_after_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_thread = Arc::clone(&cancelled);
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            cancelled_for_thread.store(true, Ordering::Release);
        });

        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::pending::<()>(),
            || cancelled.load(Ordering::Acquire),
        ));

        cancel_thread.join().unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn leading_think_block_is_stripped() {
        assert_eq!(
            strip_think_block("<think>pondering...</think>Cleaned text."),
            "Cleaned text."
        );
        assert_eq!(
            strip_think_block("  \n<think>multi\nline</think>\n  Cleaned text."),
            "Cleaned text."
        );
    }

    #[test]
    fn content_without_think_block_is_unchanged() {
        assert_eq!(strip_think_block("Cleaned text."), "Cleaned text.");
        assert_eq!(
            strip_think_block("Mentions <think> mid-sentence."),
            "Mentions <think> mid-sentence."
        );
        // Unclosed block: leave untouched rather than guess
        assert_eq!(
            strip_think_block("<think>never closed"),
            "<think>never closed"
        );
    }

    #[test]
    fn live_overlay_uses_streaming_states_only_for_streaming_models() {
        assert!(should_use_streaming_overlay(OverlayStyle::Live, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::Live, false));
        assert!(!should_use_streaming_overlay(OverlayStyle::Minimal, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::None, true));
    }
}
