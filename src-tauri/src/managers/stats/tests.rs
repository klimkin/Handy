use super::trace::*;
use super::*;
use crate::settings::get_default_settings;
use std::time::Instant;

fn manager() -> (tempfile::TempDir, Arc<StatsManager>) {
    let dir = tempfile::tempdir().unwrap();
    let stats = Arc::new(StatsManager::at(dir.path().join("stats.db")));
    (dir, stats)
}
fn config() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("model".into(), "test".into()),
        ("selected_language".into(), "en".into()),
        ("post_process".into(), "false".into()),
    ])
}
fn complete(stats: &Arc<StatsManager>, at: Instant) -> String {
    let stamp = InputStamp::at(at, InputOrigin::ShortcutCallback);
    let id = stamp.run_id.clone();
    let mut run = stats.begin(stamp, config());
    run.set_stop(at + Duration::from_secs(1));
    run.captured(CaptureEvidence {
        first_delivery: Some(at + Duration::from_millis(20)),
        valid_frames: 16000,
        ..Default::default()
    });
    run.finish(
        TerminalOutcome::OutputCompleted,
        at + Duration::from_secs(2),
        Some(OutputSettlement {
            at: at + Duration::from_secs(2),
            kind: OutputKind::ClipboardPaste,
            result: OutputResult::Completed(CompletionBasis::SynchronousOutputReturn),
        }),
    );
    id
}
#[test]
fn unknown_sample_clock_never_becomes_delivery_measurement() {
    let (_dir, stats) = manager();
    complete(&stats, Instant::now());
    let summary = stats.summary(config()).unwrap();
    let g = &summary.groups[0];
    assert!(g.capture_gap_ms.is_none());
    assert_eq!(g.capture_missing, 1);
    assert_eq!(
        g.capture_unavailable_reasons["sample_timestamps_unavailable"],
        1
    );
    assert_eq!(g.output_gap_ms.unwrap().median, 1000.0);
    stats.shutdown().unwrap();
}
#[test]
fn measured_preroll_and_overlapping_phases_use_endpoints() {
    let (_dir, stats) = manager();
    let at = Instant::now();
    let mut run = stats.begin(InputStamp::at(at, InputOrigin::ShortcutCallback), config());
    run.set_stop(at + Duration::from_millis(1000));
    run.captured(CaptureEvidence {
        first_delivery: Some(at + Duration::from_millis(16)),
        valid_frames: 20000,
        measured: Some(MeasuredCoverage {
            first: at - Duration::from_millis(8),
            through: at + Duration::from_secs(1),
            uncertainty: Duration::from_micros(100),
            discontinuities: 0,
        }),
        ..Default::default()
    });
    let trace = run.reporter();
    let phase = trace.begin_at(PhaseKind::ModelLoad, at + Duration::from_millis(10));
    trace.end(phase, PhaseOutcome::Success);
    let finalized = run.finalize(
        TerminalOutcome::OutputCompleted,
        at + Duration::from_millis(1800),
        Some(OutputSettlement {
            at: at + Duration::from_millis(1800),
            kind: OutputKind::ClipboardPaste,
            result: OutputResult::Completed(CompletionBasis::SynchronousOutputReturn),
        }),
    );
    assert_eq!(finalized.capture.first_retained_sample_us, Some(-8000));
    assert_eq!(finalized.metrics.capture_gap.us, Some(0));
    assert_eq!(finalized.metrics.output_gap.us, Some(800000));
    run.finish(TerminalOutcome::Empty, Instant::now(), None);
    stats.shutdown().unwrap();
}
#[test]
fn late_capture_discontinuity_and_missing_stop_are_explicit() {
    let (_dir, stats) = manager();
    let at = Instant::now();
    for (delay, drops, expected) in [(8, 0, Some(8000)), (0, 1, None)] {
        let mut run = stats.begin(InputStamp::at(at, InputOrigin::ShortcutCallback), config());
        run.captured(CaptureEvidence {
            valid_frames: 16000,
            dropped_frames: drops,
            measured: Some(MeasuredCoverage {
                first: at + Duration::from_millis(delay),
                through: at + Duration::from_secs(1),
                uncertainty: Duration::ZERO,
                discontinuities: drops,
            }),
            ..Default::default()
        });
        let finalized = run.finalize(TerminalOutcome::ProcessingFailed, at, None);
        assert_eq!(finalized.metrics.capture_gap.us, expected);
        assert_eq!(
            finalized.metrics.output_gap.reason.as_deref(),
            Some("stop_input_unavailable")
        );
        run.finish(TerminalOutcome::ProcessingFailed, at, None);
    }
    stats.shutdown().unwrap();
}
#[test]
fn diagnostic_overflow_retains_critical_endpoints() {
    let (_dir, stats) = manager();
    let at = Instant::now();
    let mut run = stats.begin(InputStamp::at(at, InputOrigin::ShortcutCallback), config());
    run.set_stop(at);
    for _ in 0..300 {
        run.reporter().event(at, Event::RecordingRequested);
    }
    let f = run.finalize(
        TerminalOutcome::OutputCompleted,
        at,
        Some(OutputSettlement {
            at,
            kind: OutputKind::DirectTyping,
            result: OutputResult::Completed(CompletionBasis::DirectTypingReturn),
        }),
    );
    assert_eq!(f.events.len(), 196);
    assert_eq!(f.dropped_events, 108);
    assert!(!f.events_complete);
    assert_eq!(f.metrics.output_gap.us, Some(0));
    assert!(f
        .events
        .windows(2)
        .all(|w| (w[0].t_us, w[0].seq) <= (w[1].t_us, w[1].seq)));
    run.finish(TerminalOutcome::Empty, at, None);
    stats.shutdown().unwrap();
}
#[test]
fn cancellation_empty_and_failures_remain_separate() {
    let (_dir, stats) = manager();
    for outcome in [
        TerminalOutcome::Cancelled,
        TerminalOutcome::Empty,
        TerminalOutcome::ProcessingFailed,
    ] {
        stats
            .begin(InputStamp::now(InputOrigin::ShortcutCallback), config())
            .finish(outcome, Instant::now(), None);
    }
    let s = stats.summary(config()).unwrap();
    let g = &s.groups[0];
    assert_eq!(
        (
            g.runs,
            g.cancellations,
            g.failures,
            g.capture_missing,
            g.output_missing
        ),
        (3, 1, 2, 2, 2)
    );
    stats.shutdown().unwrap();
}
#[test]
fn action_request_separates_llm_and_excludes_saved_disabled_fields() {
    let mut settings = get_default_settings();
    settings.post_process_enabled = false;
    let disabled = requested_config(&settings, "m", false);
    let enabled = requested_config(&settings, "m", true);
    assert_eq!(enabled["post_process"], "true");
    assert!(!disabled.contains_key("post_process_provider"));
    settings.post_process_provider_id = "another".into();
    assert_eq!(disabled, requested_config(&settings, "m", false));
    assert!(enabled.keys().all(|key| !key.contains("api_key")));
}
#[test]
fn migration_preserves_legacy_without_relabeling() {
    let (dir, stats) = manager();
    let conn = stats.connect().unwrap();
    conn.execute("INSERT INTO transcription_runs(timestamp,language,entry_ms,exit_ms,result,speech_detected,config) VALUES(1,'en',50,900,'text',1,'{}')",[]).unwrap();
    let summary = stats.summary(config()).unwrap();
    assert_eq!(summary.legacy_runs, 1);
    assert!(summary.groups.is_empty());
    assert_eq!(
        conn.query_row("SELECT entry_ms FROM transcription_runs", [], |r| r
            .get::<_, f64>(0))
            .unwrap(),
        50.0
    );
    stats.shutdown().unwrap();
    drop(stats);
    let reopened = StatsManager::at(dir.path().join("stats.db"));
    assert_eq!(reopened.summary(config()).unwrap().legacy_runs, 1);
    reopened.shutdown().unwrap();
}
#[test]
fn clear_rejects_old_owners_and_input_queued_before_clear() {
    let (_dir, stats) = manager();
    let old = stats.begin(stats.input_stamp(InputOrigin::ShortcutCallback), config());
    let queued = stats.input_stamp(InputOrigin::ShortcutCallback);
    complete(&stats, Instant::now());
    stats.clear().unwrap();
    old.finish(TerminalOutcome::Empty, Instant::now(), None);
    stats
        .begin(queued, config())
        .finish(TerminalOutcome::Cancelled, Instant::now(), None);
    stats.shutdown().unwrap();
    assert!(stats.summary(config()).unwrap().groups.is_empty());
    assert!(!stats.trace_path().exists());
}
#[test]
fn delayed_clear_reply_cannot_roll_back_new_recordings() {
    let (_dir, stats) = manager();
    let (reply, delayed) = mpsc::sync_channel(1);
    stats.export_tx.send(export::Command::Clear(reply)).unwrap();
    stats.clear().unwrap();
    delayed.recv().unwrap().unwrap();
    let stamp = stats.input_stamp(InputOrigin::ShortcutCallback);
    assert_eq!(stamp.generation, Some(2));
    let id = complete(&stats, Instant::now());
    stats.shutdown().unwrap();
    let trace = std::fs::read_to_string(stats.trace_path()).unwrap();
    assert!(trace.contains(&id));
    assert_eq!(stats.summary(config()).unwrap().groups[0].runs, 1);
}

#[test]
fn exporter_repairs_torn_tail_and_deduplicates_synced_pending_rows() {
    use std::io::Write;
    let (dir, stats) = manager();
    let id = complete(&stats, Instant::now());
    stats.shutdown().unwrap();
    let before = std::fs::read_to_string(stats.trace_path()).unwrap();
    assert_eq!(before.lines().count(), 1);
    stats
        .connect()
        .unwrap()
        .execute(
            "UPDATE dictation_traces SET exported=0,payload=?1 WHERE run_id=?2",
            params![before.trim(), id],
        )
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(stats.trace_path())
        .unwrap();
    file.write_all(b"{\"run_id\":\"torn").unwrap();
    file.sync_all().unwrap();
    drop(stats);
    let reopened = StatsManager::at(dir.path().join("stats.db"));
    reopened.shutdown().unwrap();
    assert_eq!(
        std::fs::read_to_string(reopened.trace_path()).unwrap(),
        before
    );
    assert_eq!(reopened.summary(config()).unwrap().groups[0].runs, 1);
}
#[test]
fn drop_preserves_forgotten_terminal_and_independent_runs() {
    let (_dir, stats) = manager();
    let first = stats.begin(InputStamp::now(InputOrigin::ShortcutCallback), config());
    complete(&stats, Instant::now());
    drop(first);
    stats.shutdown().unwrap();
    let summary = stats.summary(config()).unwrap();
    assert_eq!(summary.groups[0].runs, 2);
    let json = std::fs::read_to_string(stats.trace_path()).unwrap();
    let outcomes = json
        .lines()
        .map(|line| {
            serde_json::from_str::<FinalizedTrace>(line)
                .unwrap()
                .outcome
        })
        .collect::<Vec<_>>();
    assert!(outcomes.contains(&TerminalOutcome::UnfinishedOwner));
}
#[test]
fn output_receipt_waits_for_actual_settlement_and_disconnect_fails() {
    let (tx, rx) = std::sync::mpsc::channel();
    let ticket = OutputReceipt::pending(rx, OutputKind::ClipboardPaste);
    let at = Instant::now();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        tx.send(OutputSettlement {
            at: Instant::now(),
            kind: OutputKind::ClipboardPaste,
            result: OutputResult::Completed(CompletionBasis::ReceiptTransactionSettled),
        })
        .unwrap();
    });
    let settlement = ticket.wait();
    assert!(settlement.at.duration_since(at) >= Duration::from_millis(20));
    worker.join().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    drop(tx);
    assert!(matches!(
        OutputReceipt::pending(rx, OutputKind::ClipboardPaste)
            .wait()
            .result,
        OutputResult::Failed(OutputFailure::Disconnected)
    ));
}

#[test]
#[ignore = "requires repeatable virtual/hardware input and HANDY_TELEMETRY_SMOKE_DIR"]
fn real_recorder_telemetry_smoke() {
    use crate::audio_toolkit::audio::{
        AudioFrameCallback, CaptureSink, CaptureVad, LevelCallback, VadPolicy,
    };
    use crate::audio_toolkit::vad::{EarshotVad, SmoothedVad};
    use handy_recorder::{AudioChunk, Recorder, RecorderConfig, Sink};
    struct Probe {
        sink: CaptureSink,
        raw: Vec<f32>,
        blocks: Vec<u64>,
        valid: u64,
    }
    impl Sink for Probe {
        fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
            self.raw
                .extend_from_slice(&chunk.samples[..chunk.valid_frames]);
            self.valid += chunk.valid_frames as u64;
            let at = Instant::now();
            self.sink.process_chunk(chunk);
            if self.blocks.len() < 4096 {
                self.blocks.push(at.elapsed().as_nanos() as u64);
            }
        }
    }
    let directory = PathBuf::from(
        std::env::var_os("HANDY_TELEMETRY_SMOKE_DIR").expect("set HANDY_TELEMETRY_SMOKE_DIR"),
    );
    std::fs::create_dir_all(&directory).unwrap();
    let device = std::env::var("HANDY_TELEMETRY_SMOKE_DEVICE")
        .ok()
        .map(|name| {
            handy_recorder::list_input_devices()
                .expect("list input devices")
                .into_iter()
                .find(|device| device.name == name)
                .expect("find requested input device")
                .id
        });
    let stats = Arc::new(StatsManager::at(directory.join("stats.db")));
    let runs = std::env::var("HANDY_TELEMETRY_SMOKE_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10u32);
    let duration = Duration::from_millis(
        std::env::var("HANDY_TELEMETRY_SMOKE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000),
    );
    let mut report = Vec::new();
    for index in 0..runs {
        let enabled = index % 2 == 1;
        let stamp = stats.input_stamp(InputOrigin::ExternalCommand);
        let run_id = stamp.run_id.clone();
        let mut run = enabled.then(|| stats.begin(stamp, config()));
        let reporter = run.as_ref().map(RunOwner::reporter);
        let mic = reporter
            .as_ref()
            .map(|t| t.begin(PhaseKind::MicrophoneOpen));
        let recorder = Recorder::<Probe>::open(RecorderConfig {
            device: device.clone(),
            frames_per_chunk: Some(256),
            ..RecorderConfig::speech()
        })
        .expect("open default input");
        if let (Some(t), Some(id)) = (&reporter, mic) {
            t.end(id, PhaseOutcome::Success);
        }
        let detector = EarshotVad::new(0.5).unwrap();
        let detector = SmoothedVad::new(Box::new(detector), 2, 2, 1);
        let (tx, rx) = std::sync::mpsc::channel();
        let levels: LevelCallback = Arc::new(|_| {});
        let audio: AudioFrameCallback = Arc::new(|_| {});
        let mut sink = CaptureSink::new(
            CaptureVad::new(Box::new(detector), 2, 2),
            VadPolicy::Offline,
            levels,
            audio,
            tx,
        );
        sink.telemetry_enabled(enabled);
        if let Some(t) = &reporter {
            t.event(Instant::now(), Event::RecordingRequested);
        }
        recorder
            .start(Probe {
                sink,
                raw: Vec::with_capacity(320000),
                blocks: Vec::with_capacity(4096),
                valid: 0,
            })
            .unwrap_or_else(|error| panic!("start default input: {}", error.error));
        rx.recv_timeout(Duration::from_secs(5))
            .expect("receive first audio block");
        std::thread::sleep(duration);
        let stop = Instant::now();
        if let Some(run) = run.as_mut() {
            run.set_stop(stop);
        }
        let stopped = recorder.stop().expect("stop input");
        let capture_stopped = Instant::now();
        let drops = stopped.dropped_frames;
        let probe = stopped.sink;
        let valid = probe.valid;
        let blocks = probe.blocks;
        let raw = probe.raw;
        let captured = probe.sink.finish();
        recorder.close().unwrap();
        assert!(valid > 0, "recorder must deliver valid input");
        assert!(
            raw.iter().any(|v| v.abs() > 0.001),
            "repeatable input must contain signal"
        );
        crate::audio_toolkit::save_wav_file(
            directory.join(format!("test-only-raw-{index}.wav")),
            &raw,
        )
        .unwrap();
        crate::audio_toolkit::save_wav_file(
            directory.join(format!("test-only-vad-{index}.wav")),
            &captured.samples,
        )
        .unwrap();
        if let Some(mut run) = run {
            run.captured(CaptureEvidence {
                first_delivery: captured.first_audio,
                first_vad: captured.first_vad,
                first_vad_range: captured.first_vad_range,
                valid_frames: captured.valid_frames,
                dropped_frames: drops,
                speech_detected: captured.speech_detected,
                ..Default::default()
            });
            run.reporter().event(capture_stopped, Event::CaptureStopped);
            run.finish(TerminalOutcome::Empty, capture_stopped, None);
        }
        report.push(serde_json::json!({"index":index,"telemetry_enabled":enabled,"run_id":enabled.then_some(run_id),"valid_frames":valid,"retained_frames":captured.samples.len(),"dropped_frames":drops,"sink_block_ns":blocks,"first_vad_range":captured.first_vad_range}));
    }
    stats.shutdown().unwrap();
    std::fs::write(
        directory.join("test-only-capture-report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!(
        "SMOKE_REPORT={}",
        directory.join("test-only-capture-report.json").display()
    );
}

#[test]
fn committed_trace_survives_process_kill_and_restart_without_duplicates() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let (dir, stats) = manager();
    stats.shutdown().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "managers::stats::tests::durability_child",
            "--nocapture",
        ])
        .env("HANDY_STATS_CRASH_TEST_PATH", dir.path().join("stats.db"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let committed = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(std::result::Result::ok)
        .any(|line| line == "TRACE_COMMITTED");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(committed);
    assert_eq!(stats.summary(config()).unwrap().groups[0].runs, 1);
    drop(stats);
    let reopened = StatsManager::at(dir.path().join("stats.db"));
    reopened.shutdown().unwrap();
    assert_eq!(reopened.summary(config()).unwrap().groups[0].runs, 1);
    assert_eq!(
        std::fs::read_to_string(reopened.trace_path())
            .unwrap()
            .lines()
            .count(),
        1
    );
}
#[test]
fn durability_child() {
    use std::io::Write;
    let Some(path) = std::env::var_os("HANDY_STATS_CRASH_TEST_PATH") else {
        return;
    };
    let stats = Arc::new(StatsManager::at(path.into()));
    complete(&stats, Instant::now());
    println!("TRACE_COMMITTED");
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn oversized_metadata_is_allowlisted_bounded_and_identified_by_hash() {
    let (_dir, stats) = manager();
    let mut values = config();
    for key in [
        "model",
        "requested_model",
        "spoken_languages",
        "post_process_provider",
        "post_process_model",
    ] {
        values.insert(key.into(), "\0".repeat(100000));
    }
    values.insert("transcript".into(), "excluded secret".into());
    let run = stats.begin(InputStamp::now(InputOrigin::ShortcutCallback), values);
    let trace = run.finalize(TerminalOutcome::Empty, Instant::now(), None);
    assert!(trace.metadata_truncated);
    assert!(!trace.config.contains_key("transcript"));
    assert!(trace.config.values().all(|v| v.len() <= 128));
    assert!(trace.config["model"].contains('#'));
    assert!(serde_json::to_vec(&trace).unwrap().len() < 65536);
    run.finish(TerminalOutcome::Empty, Instant::now(), None);
    stats.shutdown().unwrap();
}
#[test]
fn llm_failure_delay_remains_inside_output_gap_and_wall_clock_changes_do_not_change_it() {
    let (_dir, stats) = manager();
    let at = Instant::now();
    let mut cfg = config();
    cfg.insert("post_process".into(), "true".into());
    let mut stamp = InputStamp::at(at, InputOrigin::ShortcutCallback);
    stamp.utc = "2001-01-01T00:00:00Z".into();
    let mut run = stats.begin(stamp, cfg);
    run.set_stop(at + Duration::from_secs(1));
    let t = run.reporter();
    let id = t.begin_at(PhaseKind::PostProcess, at + Duration::from_millis(1100));
    let attempt = t.begin_at(
        PhaseKind::PostProcessAttempt,
        at + Duration::from_millis(1100),
    );
    t.event(
        at + Duration::from_millis(1500),
        Event::PhaseCompleted {
            phase_id: attempt,
            outcome: PhaseOutcome::Failed,
        },
    );
    t.event(
        at + Duration::from_millis(1500),
        Event::PhaseCompleted {
            phase_id: id,
            outcome: PhaseOutcome::Failed,
        },
    );
    let trace = run.finalize(
        TerminalOutcome::OutputCompleted,
        at + Duration::from_millis(1600),
        Some(OutputSettlement {
            at: at + Duration::from_millis(1600),
            kind: OutputKind::DirectTyping,
            result: OutputResult::Completed(CompletionBasis::DirectTypingReturn),
        }),
    );
    assert_eq!(trace.metrics.output_gap.us, Some(600000));
    assert_eq!(trace.post_process.attempted, Some(true));
    assert_eq!(trace.post_process.outcome, "failed_original_text_fallback");
    assert_eq!(trace.started_at_utc, "2001-01-01T00:00:00Z");
    run.finish(TerminalOutcome::Empty, Instant::now(), None);
    stats.shutdown().unwrap();
}
#[test]
fn shutdown_waits_for_unique_owner_before_export_drain() {
    let (_dir, stats) = manager();
    let run = stats.begin(InputStamp::now(InputOrigin::ShortcutCallback), config());
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
    });
    stats.shutdown().unwrap();
    worker.join().unwrap();
    assert_eq!(stats.summary(config()).unwrap().pending_exports, 0);
    assert_eq!(
        std::fs::read_to_string(stats.trace_path())
            .unwrap()
            .lines()
            .count(),
        1
    );
}
#[test]
fn stop_before_activation_is_invalid_while_negative_preroll_is_preserved() {
    let (_dir, stats) = manager();
    let at = Instant::now();
    let mut run = stats.begin(InputStamp::at(at, InputOrigin::ShortcutCallback), config());
    run.set_stop(at - Duration::from_millis(1));
    let trace = run.finalize(
        TerminalOutcome::OutputCompleted,
        at,
        Some(OutputSettlement {
            at,
            kind: OutputKind::ClipboardPaste,
            result: OutputResult::Completed(CompletionBasis::SynchronousOutputReturn),
        }),
    );
    assert_eq!(trace.metrics.output_gap.us, None);
    assert_eq!(
        trace.metrics.output_gap.reason.as_deref(),
        Some("stop_before_activation")
    );
    run.finish(TerminalOutcome::Empty, at, None);
    stats.shutdown().unwrap();
}

#[test]
fn slow_output_keeps_its_owner_while_next_run_finishes() {
    let (_dir, stats) = manager();
    let at = Instant::now();
    let stamp = InputStamp::at(at, InputOrigin::ShortcutCallback);
    let first_id = stamp.run_id.clone();
    let mut first = stats.begin(stamp, config());
    first.set_stop(at);
    let (tx, rx) = std::sync::mpsc::channel();
    let output = std::thread::spawn(move || {
        let settlement = OutputReceipt::pending(rx, OutputKind::ClipboardPaste).wait();
        first.finish(
            TerminalOutcome::OutputCompleted,
            settlement.at,
            Some(settlement),
        );
    });
    let second_id = complete(&stats, at + Duration::from_millis(5));
    std::thread::sleep(Duration::from_millis(20));
    tx.send(OutputSettlement {
        at: Instant::now(),
        kind: OutputKind::ClipboardPaste,
        result: OutputResult::Completed(CompletionBasis::ReceiptTransactionSettled),
    })
    .unwrap();
    output.join().unwrap();
    stats.shutdown().unwrap();
    let traces = std::fs::read_to_string(stats.trace_path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<FinalizedTrace>(line).unwrap())
        .collect::<Vec<_>>();
    let first = traces
        .iter()
        .find(|trace| trace.run_id == first_id)
        .unwrap();
    let second = traces
        .iter()
        .find(|trace| trace.run_id == second_id)
        .unwrap();
    assert!(first.metrics.output_gap.us.unwrap() >= 20000);
    assert_eq!(second.metrics.output_gap.us, Some(1000000));
    assert_eq!(traces.len(), 2);
}
