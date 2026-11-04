use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use handy_recorder::{AudioChunk, Sink};

use super::CaptureSink;

/// Pre-roll budget: 96 ms at 16 kHz.
const HISTORY_FRAMES: usize = 1536;
/// Audio older than this at the press is not replayed, e.g. after the stream
/// stalled.
const MAX_PREROLL_AGE: Duration = Duration::from_millis(100);

/// A chunk kept for replay.
struct Packet {
    received: Instant,
    valid_frames: usize,
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
}

impl Packet {
    fn chunk(&self) -> AudioChunk<'_> {
        AudioChunk {
            samples: &self.samples,
            sample_rate: self.sample_rate,
            channels: self.channels,
            valid_frames: self.valid_frames,
        }
    }
}

/// The latest chunks, oldest first, within the pre-roll budget (at least one).
struct History {
    packets: VecDeque<Packet>,
    capacity: usize,
}

impl History {
    fn new(frames_per_chunk: usize) -> Self {
        let capacity = (HISTORY_FRAMES / frames_per_chunk).max(1);
        Self {
            packets: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, chunk: AudioChunk<'_>, received: Instant) {
        let mut samples = if self.packets.len() < self.capacity {
            Vec::with_capacity(chunk.samples.len())
        } else {
            // Reuse the oldest buffer, so an armed stream stops allocating.
            let mut oldest = self
                .packets
                .pop_front()
                .map(|packet| packet.samples)
                .unwrap_or_default();
            oldest.clear();
            oldest
        };
        samples.extend_from_slice(chunk.samples);
        self.packets.push_back(Packet {
            received,
            valid_frames: chunk.valid_frames,
            sample_rate: chunk.sample_rate,
            channels: chunk.channels,
            samples,
        });
    }

    /// Chunks received at most [`MAX_PREROLL_AGE`] before `pressed`.
    fn recent(&self, pressed: Instant) -> impl Iterator<Item = &Packet> {
        self.packets.iter().filter(move |packet| {
            pressed.saturating_duration_since(packet.received) <= MAX_PREROLL_AGE
        })
    }
}

/// A recording posted at `at`, waiting for the sink's next chunk.
struct Posted {
    recording: CaptureSink,
    at: Instant,
}

/// Hands a recording to an armed [`MicSink`]: the manager posts it, the
/// delivery thread picks it up on the next chunk without blocking.
#[derive(Clone)]
pub struct Inbox {
    pending: Arc<Mutex<Option<Posted>>>,
    frames_per_chunk: usize,
}

/// A recording [`Inbox::post`] handed back.
pub enum Refused {
    /// Another recording is waiting for pickup.
    Busy(Box<CaptureSink>),
    /// The recording's detector frame is not the microphone's chunk size.
    FrameMismatch(Box<CaptureSink>),
}

impl Refused {
    pub fn into_recording(self) -> CaptureSink {
        match self {
            Self::Busy(recording) | Self::FrameMismatch(recording) => *recording,
        }
    }
}

impl Inbox {
    /// An inbox for a microphone delivering `frames_per_chunk`-frame chunks.
    pub fn new(frames_per_chunk: usize) -> Self {
        Self {
            pending: Arc::default(),
            frames_per_chunk,
        }
    }

    /// Queues `recording` for the sink's next chunk.
    pub fn post(&self, recording: CaptureSink) -> Result<(), Refused> {
        self.post_at(recording, Instant::now())
    }

    fn post_at(&self, recording: CaptureSink, at: Instant) -> Result<(), Refused> {
        if recording.frame_samples() != self.frames_per_chunk {
            return Err(Refused::FrameMismatch(Box::new(recording)));
        }
        let mut pending = self.pending.lock().unwrap();
        if pending.is_some() {
            return Err(Refused::Busy(Box::new(recording)));
        }
        *pending = Some(Posted { recording, at });
        Ok(())
    }

    /// The posted recording, unless the manager is posting right now; the
    /// next chunk tries again, and history keeps the audio in between.
    fn try_take(&self) -> Option<Posted> {
        self.pending.try_lock().ok()?.take()
    }

    /// A posted recording no sink picked up.
    pub fn reclaim(&self) -> Option<CaptureSink> {
        self.pending
            .lock()
            .unwrap()
            .take()
            .map(|posted| posted.recording)
    }
}

/// The sink of one armed stream. It keeps the latest ~96 ms of audio, and
/// the recording posted to its [`Inbox`] gets that audio first, so it starts
/// slightly before the press. Each arm gets a fresh sink, so audio sealed
/// into one recording is never replayed into the next.
pub struct MicSink {
    inbox: Inbox,
    history: History,
    recording: Option<CaptureSink>,
}

impl MicSink {
    pub fn new(inbox: Inbox) -> Self {
        Self {
            history: History::new(inbox.frames_per_chunk),
            inbox,
            recording: None,
        }
    }

    /// The recording this sink fed, else the one posted but not picked up.
    /// Call it on the sink `stop` returned, so the final chunk has landed.
    pub fn into_recording(self) -> Option<CaptureSink> {
        self.recording.or_else(|| self.inbox.reclaim())
    }

    fn process_at(&mut self, chunk: AudioChunk<'_>, now: Instant) {
        if self.recording.is_none() {
            match self.inbox.try_take() {
                Some(posted) => self.attach(posted),
                None => {
                    self.history.push(chunk, now);
                    return;
                }
            }
        }
        if let Some(recording) = &mut self.recording {
            recording.process_chunk(chunk);
        }
    }

    /// Stored before the replay, so a recording that panics in it still comes
    /// back from `stop`.
    fn attach(&mut self, posted: Posted) {
        let replayed = self
            .history
            .recent(posted.at)
            .map(|packet| packet.valid_frames as u64)
            .sum();
        let recording = self.recording.insert(posted.recording);
        recording.set_replayed_frames(replayed);
        for packet in self.history.recent(posted.at) {
            recording.process_chunk(packet.chunk());
        }
    }
}

impl Sink for MicSink {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        self.process_at(chunk, Instant::now());
    }
}

#[cfg(test)]
mod tests;
