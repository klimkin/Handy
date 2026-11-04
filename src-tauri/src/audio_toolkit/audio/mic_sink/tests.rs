use super::{Inbox, MicSink, Refused, HISTORY_FRAMES};
use crate::audio_toolkit::audio::test_support::{ramp, recording, recording_with};
use crate::audio_toolkit::audio::{CaptureSink, VadPolicy};
use handy_recorder::AudioChunk;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Stream {
    sink: MicSink,
    inbox: Inbox,
    frames: usize,
    sent: usize,
    start: Instant,
    elapsed: Duration,
}

impl Stream {
    fn new(frames: usize) -> Self {
        let inbox = Inbox::new(frames);
        Self {
            sink: MicSink::new(inbox.clone()),
            inbox,
            frames,
            sent: 0,
            start: Instant::now(),
            elapsed: Duration::ZERO,
        }
    }

    fn chunks(&mut self, count: usize) {
        for _ in 0..count {
            self.deliver(self.frames);
        }
    }

    /// The recorder's last chunk at stop: `valid` frames, then zero padding.
    fn final_chunk(&mut self, valid: usize) {
        self.deliver(valid);
    }

    fn stall(&mut self, gap: Duration) {
        self.elapsed += gap;
    }

    fn deliver(&mut self, valid: usize) {
        let mut samples = vec![0.0; self.frames];
        for (offset, sample) in samples[..valid].iter_mut().enumerate() {
            *sample = (self.sent + offset + 1) as f32;
        }
        self.sent += valid;
        self.elapsed += Duration::from_micros(self.frames as u64 * 1_000_000 / 16_000);
        let chunk = AudioChunk {
            samples: &samples,
            sample_rate: 16_000,
            channels: 1,
            valid_frames: valid,
        };
        self.sink.process_at(chunk, self.start + self.elapsed);
    }

    fn post(&self, recording: CaptureSink) {
        assert!(
            self.inbox
                .post_at(recording, self.start + self.elapsed)
                .is_ok(),
            "post refused"
        );
    }

    fn take(self) -> CaptureSink {
        self.sink.into_recording().expect("a recording was posted")
    }
}

#[test]
fn a_recording_starts_with_the_audio_just_before_its_press() {
    let mut stream = Stream::new(512);
    stream.chunks(5);
    let (posted, ready) = recording(512);
    stream.post(posted);
    assert!(ready.try_recv().is_err());

    stream.chunks(1);

    assert!(ready.try_recv().is_ok(), "ready once audio reaches it");
    assert_eq!(
        stream.take().finish().samples,
        ramp(5 * 512 - HISTORY_FRAMES + 1, 6 * 512)
    );
}

#[test]
fn replay_is_96_ms_at_either_detector_frame_size() {
    for frames in [256, 512] {
        let mut stream = Stream::new(frames);
        stream.chunks(20);
        stream.post(recording_with(frames, VadPolicy::Offline, Arc::new(|_| {})).0);

        stream.chunks(1);

        assert_eq!(
            stream.take().finish().samples,
            ramp(20 * frames - HISTORY_FRAMES + 1, 21 * frames),
            "{frames}-frame chunks"
        );
    }
}

#[test]
fn history_older_than_100_ms_at_the_press_is_not_replayed() {
    let mut stream = Stream::new(512);
    stream.chunks(3);
    stream.stall(Duration::from_millis(80));
    stream.chunks(1);
    stream.post(recording(512).0);

    stream.chunks(1);

    assert_eq!(stream.take().finish().samples, ramp(3 * 512 + 1, 5 * 512));
}

#[test]
fn a_late_pickup_still_gets_the_audio_before_the_press() {
    let mut stream = Stream::new(512);
    stream.chunks(3);
    stream.post(recording(512).0);
    stream.stall(Duration::from_millis(80));
    stream.chunks(1);

    assert_eq!(stream.take().finish().samples, ramp(1, 4 * 512));
}

#[test]
fn a_recording_keeps_the_final_padded_chunk() {
    let mut stream = Stream::new(512);
    stream.chunks(4);
    stream.post(recording(512).0);
    stream.chunks(3);
    stream.final_chunk(100);

    assert_eq!(stream.take().finish().samples, ramp(512 + 1, 7 * 512 + 100));
}

#[test]
fn a_recording_stopped_before_any_audio_comes_back_empty() {
    let mut stream = Stream::new(512);
    stream.chunks(2);
    let (posted, ready) = recording(512);
    stream.post(posted);
    let inbox = stream.inbox.clone();

    let returned = stream.take();

    assert!(returned.finish().samples.is_empty());
    assert!(ready.recv().is_err(), "the readiness waiter wakes");
    assert!(inbox.post(recording(512).0).is_ok(), "the inbox is free");
}

#[test]
fn the_inbox_refuses_a_recording_sized_for_another_detector() {
    let inbox = Inbox::new(512);

    match inbox.post(recording(256).0) {
        Err(Refused::FrameMismatch(recording)) => assert_eq!(recording.frame_samples(), 256),
        _ => panic!("expected a frame mismatch"),
    }
    assert!(inbox.post(recording(512).0).is_ok());
}

#[test]
fn the_inbox_holds_one_recording_at_a_time() {
    let mut stream = Stream::new(512);
    let (first, first_ready) = recording(512);
    stream.post(first);

    assert!(matches!(
        stream.inbox.post(recording(512).0),
        Err(Refused::Busy(_))
    ));
    stream.chunks(1);
    assert!(first_ready.try_recv().is_ok(), "the first one is picked up");
}
