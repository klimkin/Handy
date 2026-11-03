#![cfg(test)]
#![allow(dead_code, unused_variables)]

use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

macro_rules! debug {
    ($($argument:tt)*) => {{ let _ = format_args!($($argument)*); }};
}

#[derive(Clone)]
struct App {
    effects: Arc<Mutex<Effects>>,
}

#[derive(Default)]
struct Effects {
    newer_session: bool,
    observed_cancel: bool,
    pasted: bool,
    target: Option<Cue>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cue {
    Ui,
    Chime,
    Mute,
}

impl App {
    fn set_mute(&self, _: bool) {
        self.cue(Cue::Mute);
    }
    fn cue(&self, cue: Cue) {
        let effects = self.effects.lock().unwrap();
        if effects.target == Some(cue) {
            let message = match cue {
                Cue::Ui => "ended readiness changed newer UI",
                Cue::Chime => "ended readiness played start chime",
                Cue::Mute => "ended readiness muted newer session",
            };
            assert!(!effects.newer_session, "{message}");
        }
    }
}

// External UI/audio operations are observable boundaries, each scheduled
// independently from production generation loads. Platform calls are not modeled.
mod utils {
    use super::*;
    pub fn emit_recording_ready(app: &App) {
        app.cue(Cue::Ui);
    }
    pub fn hide_recording_overlay(_: &App) {}
    pub fn paste(_: String, app: App) {
        let mut effects = app.effects.lock().unwrap();
        assert!(
            !effects.observed_cancel,
            "observed cancellation reached paste commitment"
        );
        effects.pasted = true;
    }
}

enum SoundType {
    Start,
}

fn play_feedback_sound_blocking(app: &App, _: SoundType) {
    app.cue(Cue::Chime);
}

fn get_mute() -> Option<bool> {
    Some(false)
}

struct Settings {
    mute_while_recording: bool,
}

fn get_settings(_: &App) -> Settings {
    Settings {
        mute_while_recording: true,
    }
}

struct Cancellation {
    value: AtomicU64,
    app: App,
}

impl Cancellation {
    // Instrument the actual production load. The record only notes a load
    // that returned cancellation, never cancellation merely arriving later.
    fn load(&self, ordering: Ordering) -> u64 {
        let value = self.value.load(ordering);
        if value != 0 {
            self.app.effects.lock().unwrap().observed_cancel = true;
        }
        value
    }
}

struct AudioRecordingManager {
    capture_generation: AtomicU64,
    cancel_generation: Cancellation,
    app_handle: App,
    is_open: Mutex<bool>,
    mute_state: Mutex<MuteState>,
}

// Logging/diagnostics, tray updates and ownership checks have no influence on
// the covered precommit branch. Keep extracted calls, with explicit no-op stubs.
mod diagnostics {
    use super::{Args, Name};
    pub fn end(_: (), _: Name, _: Args) {}
    pub struct Span;
    pub fn span(_: (), _: Name) -> Span {
        Span
    }
    pub fn instant(_: Option<()>, _: Name, _: Args) {}
}
enum Name {
    Run,
    Output,
    Effect,
}
enum Code {
    Cancelled,
    Start,
    Stop,
}
struct Args;
impl Args {
    fn code(_: Code) -> Self {
        Self
    }
}
enum TrayIconState {
    Idle,
}
fn set_tray_state(_: &App, _: TrayIconState) {}
struct RecordingSession;
impl RecordingSession {
    fn load(&self, _: Ordering) -> u64 {
        0
    }
}
static RECORDING_SESSION: RecordingSession = RecordingSession;
type AppHandle = App;
enum RecordingPhase {
    Started,
    Stopped,
}
fn emit_recording_phase(_: &App, phase: RecordingPhase) {
    assert!(
        !matches!(phase, RecordingPhase::Started),
        "failed start emitted started phase"
    );
}
// Executor boundary reports microphone denial for rollback checks.
fn start(_: &App, _: &str, _: &str) -> bool {
    false
}
fn stop(_: &App, _: &str, _: &str) {}

include!(concat!(env!("OUT_DIR"), "/production.rs"));

fn setup(target: Option<Cue>) -> (Arc<AudioRecordingManager>, App) {
    let app = App {
        effects: Arc::new(Mutex::new(Effects {
            target,
            ..Effects::default()
        })),
    };
    let audio = Arc::new(AudioRecordingManager {
        capture_generation: AtomicU64::new(0),
        cancel_generation: Cancellation {
            value: AtomicU64::new(0),
            app: app.clone(),
        },
        app_handle: app.clone(),
        is_open: Mutex::new(true),
        mute_state: Mutex::new(MuteState::default()),
    });
    (audio, app)
}

fn builder() -> loom::model::Builder {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;
    builder.max_branches = 1_000;
    builder.max_permutations = None;
    builder.max_duration = None;
    builder.preemption_bound = None;
    builder.checkpoint_file = None;
    builder.expect_explicit_explore = false;
    builder
}

fn readiness(target: Cue, concurrent: bool) {
    builder().check(move || {
        let (audio, app) = setup(Some(target));
        if !concurrent {
            audio.invalidate_recording_readiness();
            app.effects.lock().unwrap().newer_session = true;
        }
        let ready_audio = Arc::clone(&audio);
        let ready_app = app.clone();
        let ready = loom::thread::spawn(move || {
            production_ready(ready_audio, 0, ready_app);
        });
        if concurrent {
            let end = loom::thread::spawn(move || {
                audio.invalidate_recording_readiness();
                // User ended old capture; a newer session has acquired UI and
                // has an open microphone. Native close/open internals omitted.
                app.effects.lock().unwrap().newer_session = true;
            });
            end.join().unwrap();
        }
        ready.join().unwrap();
    });
}

#[test]
fn readiness_ui_never_changes_newer_session() {
    readiness(Cue::Ui, true);
}
#[test]
fn readiness_chime_never_follows_ended_session() {
    readiness(Cue::Chime, true);
}
#[test]
fn readiness_mute_never_changes_newer_session() {
    readiness(Cue::Mute, true);
}
#[test]
fn ended_readiness_rejects_ui() {
    readiness(Cue::Ui, false);
}
#[test]
fn ended_readiness_rejects_chime() {
    readiness(Cue::Chime, false);
}
#[test]
fn ended_readiness_rejects_mute() {
    readiness(Cue::Mute, false);
}

#[test]
fn observed_cancel_prevents_paste_commitment() {
    let outcomes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let covered = std::sync::Arc::clone(&outcomes);
    builder().check(move || {
        let (audio, app) = setup(None);
        let paste_audio = Arc::clone(&audio);
        let paste_app = app.clone();
        let paste = loom::thread::spawn(move || {
            production_paste(paste_audio, 0, paste_app, 0, "text".into());
        });
        let cancel = loom::thread::spawn(move || {
            audio.cancel_generation.value.fetch_add(1, Ordering::AcqRel);
        });
        cancel.join().unwrap();
        paste.join().unwrap();
        let effects = app.effects.lock().unwrap();
        assert!(
            !effects.observed_cancel || !effects.pasted,
            "observed cancellation reached paste commitment"
        );
        covered.fetch_or(
            if effects.observed_cancel { 1 } else { 2 },
            std::sync::atomic::Ordering::Relaxed,
        );
    });
    assert_eq!(
        outcomes.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "Loom must cover both observed cancellation and successful commitment"
    );
}

#[test]
fn cancellation_before_paste_thread_prevents_commitment() {
    builder().check(|| {
        let (audio, app) = setup(None);
        audio.cancel_generation.value.fetch_add(1, Ordering::AcqRel);
        production_paste(audio, 0, app.clone(), 0, "text".into());
        assert!(
            !app.effects.lock().unwrap().pasted,
            "cancelled generation reached paste commitment"
        );
    });
}

fn input(binding: &str, pressed: bool, now: Instant, external: bool) -> InputEvent {
    InputEvent {
        binding_id: binding.into(),
        hotkey_string: binding.into(),
        is_pressed: pressed,
        mode: ShortcutActivation::Toggle,
        hold_threshold: Duration::ZERO,
        external,
        received_at: now,
    }
}

fn busy(now: Instant) -> CoordinatorState {
    let mut state = CoordinatorState::new();
    assert!(matches!(
        state.on_input(input("a", true, now, true), now),
        Some(Effect::Start { .. })
    ));
    assert!(matches!(
        state.on_input(input("a", true, now, true), now),
        Some(Effect::Stop { .. })
    ));
    assert_eq!(state.stage, Stage::Processing);
    state
}

#[test]
fn busy_toggle_parity_exhaustive() {
    let start = Instant::now();
    let mut words = 0;
    // Every press/release word of lengths 0..8, external or keyboard. Keyboard
    // edges are 31ms apart, above production's 30ms debounce threshold.
    for external in [false, true] {
        for length in 0..=8 {
            for bits in 0..(1 << length) {
                let mut state = busy(start);
                let mut presses = 0;
                for index in 0..length {
                    let pressed = bits & (1 << index) != 0;
                    presses += usize::from(pressed);
                    let now = start + Duration::from_millis(31 * (index + 1));
                    assert!(state
                        .on_input(input("a", pressed, now, external), now)
                        .is_none());
                }
                let effect = state.on_processing_finished();
                assert_eq!(
                    effect.is_some(),
                    presses % 2 == 1,
                    "busy toggle parity changed"
                );
                if let Some(Effect::Start { binding_id, .. }) = effect {
                    assert_eq!(binding_id, "a");
                }
                words += 1;
            }
        }
    }
    assert_eq!(words, 1022);
    println!("busy parity: {words} exhaustive words");
}

#[test]
fn busy_other_binding_preserves_remembered_press() {
    let start = Instant::now();
    let mut words = 0;
    for length in 0..=8 {
        for bits in 0..(1 << length) {
            let mut state = busy(start);
            state.on_input(input("a", true, start, true), start);
            for index in 0..length {
                state.on_input(input("b", bits & (1 << index) != 0, start, true), start);
                assert_eq!(
                    state.pending_press.as_ref().map(|p| p.binding_id.as_str()),
                    Some("a"),
                    "another binding replaced remembered press"
                );
            }
            assert!(
                matches!(state.on_processing_finished(), Some(Effect::Start { binding_id, .. }) if binding_id == "a"),
                "another binding replaced remembered press"
            );
            words += 1;
        }
    }
    assert_eq!(words, 511);
    println!("binding isolation: {words} exhaustive words");
}

#[test]
fn busy_cancel_removes_pending_start() {
    let start = Instant::now();
    let mut words = 0;
    // All two-binding press/release words of lengths 0..6. Cancel occurs at
    // every possible boundary by checking each prefix before drain.
    for length in 0..=6 {
        for mut word in 0..4_usize.pow(length) {
            let mut state = busy(start);
            for _ in 0..length {
                let symbol = word % 4;
                word /= 4;
                state.on_input(
                    input(
                        if symbol < 2 { "a" } else { "b" },
                        symbol % 2 == 0,
                        start,
                        true,
                    ),
                    start,
                );
            }
            state.on_cancel(false);
            assert_eq!(state.stage, Stage::Processing);
            assert!(
                state.on_processing_finished().is_none(),
                "cancel retained pending start"
            );
            words += 1;
        }
    }
    assert_eq!(words, 5461);
    println!("cancel pending: {words} exhaustive prefixes");
}

#[test]
fn failed_start_rolls_coordinator_back() {
    builder().check(|| {
        let (_, app) = setup(None);
        let now = Instant::now();
        for mode in [
            ShortcutActivation::Toggle,
            ShortcutActivation::PushToTalk,
            ShortcutActivation::HoldOrToggle,
        ] {
            for binding in ["a", "b"] {
                let mut state = CoordinatorState::new();
                let mut event = input(binding, true, now, true);
                event.mode = mode;
                let effect = state.on_input(event, now).expect("initial start effect");
                assert!(matches!(effect, Effect::Start { .. }));
                run_effect(&app, &mut state, effect);
                assert_eq!(
                    state.stage,
                    Stage::Idle,
                    "failed start left coordinator recording"
                );
                assert!(state.hold.is_none(), "failed start retained hold");
                assert!(
                    matches!(
                        state.on_input(input(binding, true, now, true), now),
                        Some(Effect::Start { .. })
                    ),
                    "failed start prevented retry"
                );
            }
        }
    });
}
