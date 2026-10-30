use super::{AudioFrameCallback, CaptureSink, CaptureVad, LevelCallback, VadPolicy};
use crate::audio_toolkit::vad::{VadFrame, VoiceActivityDetector};
use handy_recorder::{AudioChunk, Sink};
use std::sync::{mpsc, Arc, Mutex};

#[derive(Default)]
struct ScriptedVad {
    frame_samples: usize,
    hangover: Arc<Mutex<Option<usize>>>,
    resets: Arc<Mutex<usize>>,
}

impl VoiceActivityDetector for ScriptedVad {
    fn push_frame<'a>(&'a mut self, frame: &'a [f32]) -> anyhow::Result<VadFrame<'a>> {
        if frame[0] < 0.0 {
            anyhow::bail!("scripted detector failure");
        }
        Ok(if frame[0] != 0.0 {
            VadFrame::Speech(frame)
        } else {
            VadFrame::Noise
        })
    }

    fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    fn set_hangover_frames(&mut self, frames: usize) {
        *self.hangover.lock().unwrap() = Some(frames);
    }

    fn reset(&mut self) {
        *self.resets.lock().unwrap() += 1;
    }
}

const FRAME: usize = 4;

struct Harness {
    sink: CaptureSink,
    ready: mpsc::Receiver<()>,
    fed: Arc<Mutex<Vec<f32>>>,
    hangover: Arc<Mutex<Option<usize>>>,
    resets: Arc<Mutex<usize>>,
}

fn harness(policy: VadPolicy) -> Harness {
    let vad = ScriptedVad {
        frame_samples: FRAME,
        ..Default::default()
    };
    let hangover = Arc::clone(&vad.hangover);
    let resets = Arc::clone(&vad.resets);
    let fed = Arc::new(Mutex::new(Vec::new()));
    let on_audio: AudioFrameCallback = {
        let fed = Arc::clone(&fed);
        Arc::new(move |frame: &[f32]| fed.lock().unwrap().extend_from_slice(frame))
    };
    let on_levels: LevelCallback = Arc::new(|_| {});
    let (ready_tx, ready) = mpsc::channel();
    let sink = CaptureSink::new(
        CaptureVad::new(Box::new(vad), 10, 20),
        policy,
        on_levels,
        on_audio,
        ready_tx,
    );
    Harness {
        sink,
        ready,
        fed,
        hangover,
        resets,
    }
}

fn chunk(samples: &[f32], valid_frames: usize) -> AudioChunk<'_> {
    AudioChunk {
        samples,
        sample_rate: 16_000,
        channels: 1,
        valid_frames,
    }
}

#[test]
fn readiness_fires_once_on_the_first_chunk() {
    let mut h = harness(VadPolicy::Disabled);
    assert!(h.ready.try_recv().is_err());
    assert!(h.sink.first_audio.is_none());

    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    let first_audio = h.sink.first_audio;
    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    assert!(first_audio.is_some());
    assert_eq!(h.sink.first_audio, first_audio);

    assert!(h.ready.try_recv().is_ok());
    // The sender is gone after the first chunk, so a second signal can't come.
    assert_eq!(h.ready.try_recv(), Err(mpsc::TryRecvError::Disconnected));
}

#[test]
fn readiness_waiter_wakes_when_a_recording_ends_without_audio() {
    let h = harness(VadPolicy::Offline);
    assert!(h.sink.finish().first_audio.is_none());
    assert!(h.ready.recv().is_err());
}

#[test]
fn disabled_vad_keeps_every_real_sample_and_drops_padding() {
    let mut h = harness(VadPolicy::Disabled);
    h.sink.process_chunk(chunk(&[1.0, 0.0, 2.0, 0.0], FRAME));
    // Final chunk: one real frame, three of zero padding.
    h.sink.process_chunk(chunk(&[3.0, 0.0, 0.0, 0.0], 1));

    let expected = vec![1.0, 0.0, 2.0, 0.0, 3.0];
    assert_eq!(*h.fed.lock().unwrap(), expected);
    assert_eq!(h.sink.finish().samples, expected);
    assert_eq!(
        *h.resets.lock().unwrap(),
        0,
        "a disabled policy leaves VAD alone"
    );
}

#[test]
fn a_recording_of_only_zeros_is_silent_input() {
    let mut h = harness(VadPolicy::Offline);
    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    assert!(h.sink.finish().input_silent);
}

#[test]
fn a_recording_stopped_before_any_audio_is_not_silent_input() {
    // A quick tap that ends before the device delivers its first chunk says
    // nothing about the microphone.
    let h = harness(VadPolicy::Offline);
    assert!(!h.sink.finish().input_silent);
}

#[test]
fn sound_counts_even_when_vad_drops_it() {
    let mut h = harness(VadPolicy::Offline);
    // Noise to the detector (its first sample is zero), but not silence.
    h.sink.process_chunk(chunk(&[0.0, 0.01, 0.0, 0.0], FRAME));
    let captured = h.sink.finish();
    assert!(captured.samples.is_empty());
    assert!(!captured.input_silent);
}

#[test]
fn vad_keeps_speech_frames_only() {
    let mut h = harness(VadPolicy::Offline);
    h.sink.process_chunk(chunk(&[1.0; FRAME], FRAME));
    h.sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    h.sink.process_chunk(chunk(&[2.0; FRAME], FRAME));

    let expected = [[1.0; FRAME], [2.0; FRAME]].concat();
    assert_eq!(*h.fed.lock().unwrap(), expected);
    assert_eq!(h.sink.finish().samples, expected);
}

#[test]
fn only_a_frame_the_detector_judged_speech_is_speech_evidence() {
    let speech_detected = |policy: VadPolicy, frames: &[[f32; FRAME]]| {
        let mut h = harness(policy);
        for frame in frames {
            h.sink.process_chunk(chunk(frame, FRAME));
        }
        let captured = h.sink.finish();
        (captured.samples.len(), captured.speech_detected)
    };

    assert_eq!(
        speech_detected(VadPolicy::Disabled, &[[1.0; FRAME]]),
        (FRAME, false),
        "with the VAD off nothing is known about the audio"
    );
    assert_eq!(
        speech_detected(VadPolicy::Offline, &[[0.0; FRAME]]),
        (0, false)
    );
    assert_eq!(
        speech_detected(VadPolicy::Offline, &[[0.0; FRAME], [1.0; FRAME]]),
        (FRAME, true)
    );
    assert_eq!(
        speech_detected(VadPolicy::Streaming, &[[-1.0; FRAME]]),
        (FRAME, false),
        "a frame the detector failed on is kept but is not speech"
    );
}

#[test]
fn each_recording_resets_the_detector_with_its_policys_hangover() {
    let offline = harness(VadPolicy::Offline);
    assert_eq!(*offline.hangover.lock().unwrap(), Some(10));
    assert_eq!(*offline.resets.lock().unwrap(), 1);

    let streaming = harness(VadPolicy::Streaming);
    assert_eq!(*streaming.hangover.lock().unwrap(), Some(20));
    assert_eq!(*streaming.resets.lock().unwrap(), 1);
}

#[test]
fn finish_returns_the_detector_for_the_next_recording() {
    let h = harness(VadPolicy::Offline);
    let captured = h.sink.finish();
    assert_eq!(captured.vad.frame_samples(), FRAME);
    assert_eq!(captured.vad.hangover_for(VadPolicy::Streaming), 20);
}

#[test]
fn raw_coverage_counts_valid_frames_and_fallback_lineage_excludes_padding() {
    let mut h = harness(VadPolicy::Streaming);
    h.sink.process_chunk(chunk(&[-1.0, 1.0, 0.0, 0.0], 2));
    let audio = h.sink.finish();
    assert_eq!(audio.valid_frames, 2);
    assert_eq!(audio.first_vad_range, Some((0, 2)));
    assert_eq!(audio.samples.len(), FRAME);
    assert!(audio.first_audio.is_some());
}
#[test]
fn smoothed_vad_preserves_prefill_origin_and_partial_last_range() {
    let scripted = ScriptedVad {
        frame_samples: FRAME,
        ..Default::default()
    };
    let smoothed = crate::audio_toolkit::vad::SmoothedVad::new(Box::new(scripted), 2, 0, 1);
    let (tx, _) = mpsc::channel();
    let mut sink = CaptureSink::new(
        CaptureVad::new(Box::new(smoothed), 0, 0),
        VadPolicy::Offline,
        Arc::new(|_| {}),
        Arc::new(|_| {}),
        tx,
    );
    sink.process_chunk(chunk(&[0.0; FRAME], FRAME));
    sink.process_chunk(chunk(&[1.0, 1.0, 0.0, 0.0], 2));
    let audio = sink.finish();
    assert_eq!(audio.valid_frames, 6);
    assert_eq!(audio.first_vad_range, Some((0, 6)));
    assert_eq!(audio.samples.len(), 8);
}
#[test]
fn unproven_detector_output_does_not_fabricate_source_range() {
    let mut h = harness(VadPolicy::Offline);
    h.sink.process_chunk(chunk(&[1.0; FRAME], FRAME));
    let audio = h.sink.finish();
    assert!(audio.first_vad.is_some());
    assert!(audio.first_vad_range.is_none());
}

#[test]
fn cancel_drain_keeps_capture_after_owner_moves_to_stop_worker() {
    use crate::managers::stats::trace::{FinalizedTrace, InputOrigin, TerminalOutcome};
    use crate::managers::stats::StatsManager;
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    let stats = Arc::new(StatsManager::test_at(dir.path().join("stats.db")));
    let mut run = stats.begin(
        stats.input_stamp(InputOrigin::ShortcutCallback),
        Default::default(),
    );
    let mut h = harness(VadPolicy::Offline);
    h.sink = h.sink.with_trace(Some(run.reporter()));
    h.sink.process_chunk(chunk(&[1.0; FRAME], FRAME));
    run.set_stop(Instant::now());
    let (resume, drain_complete) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        drain_complete.recv().unwrap();
        run.finish(TerminalOutcome::Cancelled, Instant::now(), None);
    });

    let captured = h.sink.finish();
    let delivery = captured.first_audio;
    let _ = captured.publish_capture(3, false);
    drop(captured);
    resume.send(()).unwrap();
    worker.join().unwrap();
    stats.shutdown().unwrap();

    let json = std::fs::read_to_string(stats.trace_path()).unwrap();
    let trace: FinalizedTrace = serde_json::from_str(json.trim()).unwrap();
    assert!(delivery.is_some());
    assert!(trace.capture.first_delivery_us.is_some());
    assert_eq!(trace.capture.valid_frames, FRAME as u64);
    assert_eq!(trace.capture.dropped_frames, 3);
    assert!(trace.capture.speech_detected);
    assert!(matches!(trace.outcome, TerminalOutcome::Cancelled));
    assert!(trace.events.iter().any(|event| matches!(
        event.event,
        crate::managers::stats::trace::Event::CaptureStopped
    )));
    assert!(trace.output.is_none());
    assert!(trace.metrics.output_gap.us.is_none());
}
