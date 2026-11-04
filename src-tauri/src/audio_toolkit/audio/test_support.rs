use std::sync::{mpsc, Arc};

use super::{AudioFrameCallback, CaptureSink, CaptureVad, VadPolicy};
use crate::audio_toolkit::vad::{VadFrame, VoiceActivityDetector};
use crate::diagnostics;

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

pub(super) fn recording(frames: usize) -> (CaptureSink, mpsc::Receiver<()>) {
    recording_with(frames, VadPolicy::Disabled, Arc::new(|_| {}))
}

pub(super) fn recording_with(
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

pub(super) fn ramp(first: usize, last: usize) -> Vec<f32> {
    (first..=last).map(|i| i as f32).collect()
}
