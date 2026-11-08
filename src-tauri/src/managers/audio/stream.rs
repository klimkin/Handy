use std::sync::Arc;

use log::warn;

use super::MicrophoneMode;
use crate::audio_toolkit::audio::OpenMicrophone;

/// What the microphone does between takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IdleStream {
    /// Open and armed, so a take starts with the audio before the press.
    Listening,
    /// Open, not armed.
    Open,
    /// Left open if it is, until the lazy close.
    Lingering,
    Closed,
}

impl IdleStream {
    /// A listening recorder holds an AirPods headset in its call profile, so
    /// AirPods mode keeps the stream open without arming it.
    pub(super) fn new(mode: MicrophoneMode, airpods: bool, lazy_close: bool) -> Self {
        match mode {
            MicrophoneMode::AlwaysOn if airpods => Self::Open,
            MicrophoneMode::AlwaysOn => Self::Listening,
            MicrophoneMode::OnDemand if lazy_close => Self::Lingering,
            MicrophoneMode::OnDemand => Self::Closed,
        }
    }
}

/// The manager's open microphone. A take holds its own handle, so closing
/// the stream mid-take leaves the take's device running until it ends.
#[derive(Default)]
pub(super) struct Stream {
    microphone: Option<Arc<OpenMicrophone>>,
}

impl Stream {
    pub(super) fn is_open(&self) -> bool {
        self.microphone.is_some()
    }

    pub(super) fn close(&mut self) {
        self.microphone = None;
    }

    /// The open microphone, after dropping it if it has failed.
    fn healthy(&mut self) -> Option<Arc<OpenMicrophone>> {
        if self.microphone.as_ref().is_some_and(|m| m.needs_reopen()) {
            warn!("Microphone stream failed; closing it");
            self.close();
        }
        self.microphone.clone()
    }

    pub(super) fn ensure_open<E>(
        &mut self,
        open: impl FnOnce() -> Result<OpenMicrophone, E>,
    ) -> Result<Arc<OpenMicrophone>, E> {
        if let Some(microphone) = self.healthy() {
            return Ok(microphone);
        }
        let microphone = Arc::new(open()?);
        self.microphone = Some(Arc::clone(&microphone));
        Ok(microphone)
    }

    /// Brings an idle stream to `want`, whatever it was doing before.
    pub(super) fn settle<E>(
        &mut self,
        want: IdleStream,
        open: impl FnOnce() -> Result<OpenMicrophone, E>,
    ) -> Result<(), E> {
        match want {
            IdleStream::Listening => {
                if let Err(error) = self.ensure_open(open)?.arm() {
                    warn!(
                        "Microphone failed between recordings ({error}); the next one reopens it"
                    );
                }
            }
            IdleStream::Open => drop(self.ensure_open(open)?.seal()),
            IdleStream::Lingering => {
                if let Some(microphone) = self.healthy() {
                    drop(microphone.seal());
                }
            }
            IdleStream::Closed => self.close(),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use handy_recorder::testing::FakeMic;

    use super::{IdleStream, Stream};
    use crate::audio_toolkit::audio::test_support::{
        open, play, ramp, recording, samples, while_running, BLOCK_FRAMES, FRAMES, HISTORY_FRAMES,
        SETTLE,
    };
    use crate::audio_toolkit::audio::OpenMicrophone;
    use crate::managers::audio::MicrophoneMode;

    const EVERY: [IdleStream; 4] = [
        IdleStream::Listening,
        IdleStream::Open,
        IdleStream::Lingering,
        IdleStream::Closed,
    ];

    fn no_reopen() -> anyhow::Result<OpenMicrophone> {
        panic!("the stream reopened")
    }

    /// Settles the stream while the device calls back, as a live one does,
    /// and counts the opens.
    fn settle(stream: &mut Stream, fake: &FakeMic, want: IdleStream) -> usize {
        let opens = AtomicUsize::new(0);
        while_running(fake, || {
            stream.settle(want, || {
                opens.fetch_add(1, Ordering::Relaxed);
                anyhow::Ok(open(fake))
            })
        })
        .expect("the fake microphone opens");
        thread::sleep(SETTLE);
        opens.into_inner()
    }

    /// A press and release as the manager runs them: the take holds its own
    /// handle until it is sealed.
    fn take(stream: &mut Stream, fake: &FakeMic, first: usize, frames: usize) -> Vec<f32> {
        let microphone = stream
            .ensure_open(|| anyhow::Ok(open(fake)))
            .expect("the fake microphone opens");
        assert!(microphone.post(recording(FRAMES).0).is_ok(), "post refused");
        microphone.arm().expect("the microphone arms");
        thread::sleep(SETTLE);
        play(fake, 1, first, frames);
        samples(while_running(fake, || microphone.seal()))
    }

    /// What the idle stream was doing, read from the next take: a listening
    /// one hands it the audio before the press.
    fn observe(stream: &mut Stream, fake: &FakeMic) -> IdleStream {
        if !fake.is_streaming() {
            return IdleStream::Closed;
        }
        let before = 3_200;
        play(fake, 1, 1, before);
        thread::sleep(SETTLE);
        stream
            .ensure_open(no_reopen)
            .expect("the stream is healthy");
        let audio = take(stream, fake, before + 1, BLOCK_FRAMES);
        if audio == ramp(before + 1, before + BLOCK_FRAMES) {
            return IdleStream::Open;
        }
        assert_eq!(
            audio,
            ramp(
                before / FRAMES * FRAMES - HISTORY_FRAMES + 1,
                before + BLOCK_FRAMES
            )
        );
        IdleStream::Listening
    }

    fn after_settling(want: IdleStream, was_open: bool) -> IdleStream {
        match want {
            IdleStream::Lingering if was_open => IdleStream::Open,
            IdleStream::Lingering => IdleStream::Closed,
            want => want,
        }
    }

    #[test]
    fn the_last_settle_decides_the_stream() {
        for first in EVERY {
            for last in EVERY {
                let fake = FakeMic::new(16_000, 1);
                let mut stream = Stream::default();
                settle(&mut stream, &fake, first);
                let was_open = fake.is_streaming();

                let opens = settle(&mut stream, &fake, last);

                let observed = after_settling(last, was_open);
                assert_eq!(
                    observe(&mut stream, &fake),
                    observed,
                    "{first:?} then {last:?}"
                );
                let reopened = !was_open && observed != IdleStream::Closed;
                assert_eq!(opens, usize::from(reopened), "{first:?} then {last:?}");
            }
        }
    }

    #[test]
    fn the_stream_settles_by_the_mode_at_the_end_of_a_take() {
        for first in EVERY {
            for last in EVERY {
                let fake = FakeMic::new(16_000, 1);
                let mut stream = Stream::default();
                settle(&mut stream, &fake, first);
                assert_eq!(take(&mut stream, &fake, 1, FRAMES), ramp(1, FRAMES));

                assert_eq!(
                    settle(&mut stream, &fake, last),
                    0,
                    "the take's stream is reused"
                );

                assert_eq!(
                    observe(&mut stream, &fake),
                    after_settling(last, true),
                    "{first:?} then {last:?}"
                );
            }
        }
    }

    #[test]
    fn a_device_change_mid_take_waits_for_the_take_to_end() {
        let old = FakeMic::new(16_000, 1);
        let new = FakeMic::new(16_000, 1);
        let mut stream = Stream::default();
        settle(&mut stream, &old, IdleStream::Listening);
        let microphone = stream.ensure_open(no_reopen).unwrap();
        assert!(microphone.post(recording(FRAMES).0).is_ok(), "post refused");
        microphone.arm().expect("the microphone arms");
        thread::sleep(SETTLE);
        play(&old, 1, 1, FRAMES);

        stream.close();

        assert!(old.is_streaming(), "the take keeps its device");
        play(&old, 1, FRAMES + 1, FRAMES);
        let audio = samples(while_running(&old, || microphone.seal()));
        drop(microphone);
        assert_eq!(audio, ramp(1, 2 * FRAMES), "the take keeps all its audio");
        assert!(!old.is_streaming(), "the old device closes with the take");
        settle(&mut stream, &new, IdleStream::Listening);
        assert_eq!(observe(&mut stream, &new), IdleStream::Listening);
    }

    #[test]
    fn airpods_mode_never_listens_between_takes() {
        for lazy_close in [false, true] {
            assert_eq!(
                IdleStream::new(MicrophoneMode::AlwaysOn, true, lazy_close),
                IdleStream::Open
            );
            assert_eq!(
                IdleStream::new(MicrophoneMode::AlwaysOn, false, lazy_close),
                IdleStream::Listening
            );
        }
    }
}
