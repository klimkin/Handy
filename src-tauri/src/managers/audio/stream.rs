use std::sync::Arc;
use std::time::{Duration, Instant};

use handy_recorder::ErrorKind;
use log::warn;

use super::MicrophoneMode;
use crate::audio_toolkit::audio::OpenMicrophone;

/// How often a failed idle stream reopens on its own. A take always reopens.
const RECOVERY_INTERVAL: Duration = Duration::from_secs(60);

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
    last_recovery: Option<Instant>,
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

    /// Settles again after the idle stream failed on its own, at most once
    /// per `RECOVERY_INTERVAL`, so a device that keeps failing cannot hold
    /// the state lock in a reopen loop. A lost device is left to the next
    /// take: reopening falls back to the default device, which replaces the
    /// user's choice. Revoked access would only fail again.
    pub(super) fn recover<E>(
        &mut self,
        failure: ErrorKind,
        now: Instant,
        want: IdleStream,
        open: impl FnOnce() -> Result<OpenMicrophone, E>,
    ) -> Result<(), E> {
        let failed = self.microphone.as_ref().is_some_and(|m| m.needs_reopen());
        let transient = !matches!(failure, ErrorKind::DeviceLost | ErrorKind::PermissionDenied);
        let due = self
            .last_recovery
            .is_none_or(|last| now.duration_since(last) >= RECOVERY_INTERVAL);
        if !(failed && transient && due) {
            return Ok(());
        }
        self.last_recovery = Some(now);
        self.settle(want, open)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use handy_recorder::testing::{FakeError, FakeMic};
    use handy_recorder::ErrorKind;

    use super::{IdleStream, Stream, RECOVERY_INTERVAL};
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

    /// Runs `step` while the device calls back, as a live one does, and
    /// counts the microphones it opens.
    fn count_opens(
        fake: &FakeMic,
        step: impl FnOnce(&AtomicUsize) -> anyhow::Result<()> + Send,
    ) -> usize {
        let opens = AtomicUsize::new(0);
        while_running(fake, || step(&opens)).expect("the fake microphone opens");
        thread::sleep(SETTLE);
        opens.into_inner()
    }

    fn opener<'a>(
        fake: &'a FakeMic,
        opens: &'a AtomicUsize,
    ) -> impl FnOnce() -> anyhow::Result<OpenMicrophone> + 'a {
        move || {
            opens.fetch_add(1, Ordering::Relaxed);
            anyhow::Ok(open(fake))
        }
    }

    fn settle(stream: &mut Stream, fake: &FakeMic, want: IdleStream) -> usize {
        count_opens(fake, |opens| stream.settle(want, opener(fake, opens)))
    }

    fn recover(stream: &mut Stream, fake: &FakeMic, failure: ErrorKind, now: Instant) -> usize {
        count_opens(fake, |opens| {
            stream.recover(failure, now, IdleStream::Listening, opener(fake, opens))
        })
    }

    /// Fails the idle stream's device and waits for the stream to see it.
    fn fail(stream: &Stream, fake: &FakeMic, error: FakeError) {
        assert!(fake.report_error(error), "the device is streaming");
        let microphone = stream.microphone.as_ref().expect("the stream is open");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !microphone.needs_reopen() {
            assert!(Instant::now() < deadline, "the failure never arrived");
            thread::yield_now();
        }
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
    fn a_failed_idle_stream_recovers_at_most_once_per_interval() {
        let fake = FakeMic::new(16_000, 1);
        let mut stream = Stream::default();
        settle(&mut stream, &fake, IdleStream::Listening);
        let start = Instant::now();
        assert_eq!(
            recover(&mut stream, &fake, ErrorKind::Backend, start),
            0,
            "a healthy stream is left alone"
        );

        fail(&stream, &fake, FakeError::Other);
        assert_eq!(recover(&mut stream, &fake, ErrorKind::Backend, start), 1);
        assert_eq!(observe(&mut stream, &fake), IdleStream::Listening);

        settle(&mut stream, &fake, IdleStream::Listening);
        fail(&stream, &fake, FakeError::Other);
        let soon = start + RECOVERY_INTERVAL / 2;
        assert_eq!(
            recover(&mut stream, &fake, ErrorKind::Backend, soon),
            0,
            "a second failure within the interval waits"
        );
        let later = start + RECOVERY_INTERVAL;
        assert_eq!(recover(&mut stream, &fake, ErrorKind::Backend, later), 1);
        assert_eq!(observe(&mut stream, &fake), IdleStream::Listening);
    }

    #[test]
    fn a_lost_or_forbidden_microphone_waits_for_the_next_take() {
        for (error, failure) in [
            (FakeError::DeviceNotAvailable, ErrorKind::DeviceLost),
            (FakeError::PermissionDenied, ErrorKind::PermissionDenied),
        ] {
            let fake = FakeMic::new(16_000, 1);
            let mut stream = Stream::default();
            settle(&mut stream, &fake, IdleStream::Listening);
            fail(&stream, &fake, error);

            assert_eq!(
                recover(&mut stream, &fake, failure, Instant::now()),
                0,
                "{failure:?}"
            );

            let audio = take(&mut stream, &fake, 1, FRAMES);
            assert_eq!(
                audio,
                ramp(1, FRAMES),
                "the next take reopens after {failure:?}"
            );
        }
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
