use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use handy_recorder::testing::FakeMic;
use handy_recorder::RecorderConfig;

pub(crate) use super::mic_sink::HISTORY_FRAMES;
use super::{AudioFrameCallback, CaptureSink, CaptureVad, OpenMicrophone, Sealed, VadPolicy};
use crate::audio_toolkit::vad::{VadFrame, VoiceActivityDetector};
use crate::diagnostics;

pub(crate) const FRAMES: usize = 512;
pub(crate) const BLOCK_FRAMES: usize = 160;
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(2);
/// Time for the recorder's delivery thread to apply a start, or to file the
/// audio pushed so far: a block pushed before the start applies is discarded.
pub(crate) const SETTLE: Duration = Duration::from_millis(20);

struct FrameSized(usize);

impl VoiceActivityDetector for FrameSized {
    fn push_frame<'a>(&'a mut self, frame: &'a [f32]) -> anyhow::Result<VadFrame<'a>> {
        assert_eq!(frame.len(), self.0, "chunk is not one detector frame");
        Ok(VadFrame::Speech(frame))
    }

    fn frame_samples(&self) -> usize {
        self.0
    }
}

pub(crate) fn recording(frames: usize) -> (CaptureSink, mpsc::Receiver<()>) {
    recording_with(frames, VadPolicy::Disabled, Arc::new(|_| {}))
}

pub(crate) fn recording_with(
    frames: usize,
    policy: VadPolicy,
    on_audio: AudioFrameCallback,
) -> (CaptureSink, mpsc::Receiver<()>) {
    let (ready_tx, ready) = mpsc::channel();
    let recording = CaptureSink::new(
        CaptureVad::new(Box::new(FrameSized(frames)), 0, 0),
        policy,
        Arc::new(|_| {}),
        on_audio,
        ready_tx,
        diagnostics::next_run(),
    );
    (recording, ready)
}

pub(crate) fn ramp(first: usize, last: usize) -> Vec<f32> {
    (first..=last).map(|i| i as f32).collect()
}

pub(crate) fn config() -> RecorderConfig {
    RecorderConfig {
        frames_per_chunk: Some(FRAMES),
        ..RecorderConfig::speech()
    }
}

pub(crate) fn open(fake: &FakeMic) -> OpenMicrophone {
    OpenMicrophone::open_with(
        |_| {},
        |on_failure| fake.open(config(), Some(on_failure), OPEN_TIMEOUT),
    )
    .expect("the fake microphone opens")
}

pub(crate) fn play(fake: &FakeMic, channels: u16, first: usize, frames: usize) {
    let samples: Vec<f32> = (first..first + frames)
        .flat_map(|i| std::iter::repeat_n(i as f32, channels.into()))
        .collect();
    for block in samples.chunks(BLOCK_FRAMES * usize::from(channels)) {
        assert!(fake.push(block), "the device is streaming");
    }
}

/// Runs `stop` while the device keeps calling back, as a live device does:
/// a stop waits for the next callback.
pub(crate) fn while_running<T: Send>(fake: &FakeMic, stop: impl FnOnce() -> T + Send) -> T {
    thread::scope(|scope| {
        let stopping = scope.spawn(stop);
        while !stopping.is_finished() {
            fake.push(&[]);
            thread::yield_now();
        }
        stopping.join().unwrap()
    })
}

pub(crate) fn samples(sealed: Sealed) -> Vec<f32> {
    sealed
        .recording
        .expect("the posted recording comes back")
        .finish()
        .samples
}
