use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use handy_recorder::testing::{FakeError, FakeMic};
use handy_recorder::{EndReason, Recorder};

use super::{OpenMicrophone, Sealed};
use crate::audio_toolkit::audio::mic_sink::HISTORY_FRAMES;
use crate::audio_toolkit::audio::test_support::{
    config, open, play, ramp, recording, recording_with, samples, while_running, BLOCK_FRAMES,
    FRAMES, OPEN_TIMEOUT, SETTLE,
};
use crate::audio_toolkit::audio::{CaptureSink, VadPolicy};

fn arm(mic: &OpenMicrophone) {
    mic.arm().expect("the microphone arms");
    thread::sleep(SETTLE);
}

fn post(mic: &OpenMicrophone, recording: CaptureSink) {
    assert!(mic.post(recording).is_ok(), "post refused");
}

fn sink_panicked(sealed: &Sealed) -> bool {
    matches!(
        &sealed.end,
        Ok(end) if matches!(&end.reason, EndReason::SinkPanicked(message) if message == "scripted sink failure")
    )
}

fn panicking_recording() -> CaptureSink {
    recording_with(
        FRAMES,
        VadPolicy::Disabled,
        Arc::new(|_| panic!("scripted sink failure")),
    )
    .0
}

fn ordinary_take(rate: u32, channels: u16, first: usize, frames: usize) -> Vec<f32> {
    let fake = FakeMic::new(rate, channels);
    let recorder: Recorder<CaptureSink> = fake.open(config(), None, OPEN_TIMEOUT).unwrap();
    let take = recording_with(FRAMES, VadPolicy::Offline, Arc::new(|_| {})).0;
    assert!(recorder.start(take).is_ok());
    thread::sleep(SETTLE);
    play(&fake, channels, first, frames);
    let stopped = while_running(&fake, || recorder.stop()).unwrap();
    stopped.sink.finish().samples
}

#[test]
fn a_take_ends_exactly_like_an_ordinary_recording() {
    for (rate, channels) in [(16_000, 1), (48_000, 2)] {
        let fake = FakeMic::new(rate, channels);
        let mic = open(&fake);
        post(
            &mic,
            recording_with(FRAMES, VadPolicy::Offline, Arc::new(|_| {})).0,
        );
        arm(&mic);
        play(&fake, channels, 1, 4_000);

        let take = samples(while_running(&fake, || mic.seal()));

        assert_eq!(take.len() % FRAMES, 0);
        assert_eq!(
            take,
            ordinary_take(rate, channels, 1, 4_000),
            "{rate} Hz, {channels} channels"
        );
    }
}

#[test]
fn an_always_on_take_ends_exactly_like_an_ordinary_recording() {
    let fake = FakeMic::new(16_000, 1);
    let mic = open(&fake);
    arm(&mic);
    play(&fake, 1, 1, 4 * FRAMES);
    thread::sleep(SETTLE);
    post(
        &mic,
        recording_with(FRAMES, VadPolicy::Offline, Arc::new(|_| {})).0,
    );
    play(&fake, 1, 4 * FRAMES + 1, 4_000);

    let take = samples(while_running(&fake, || mic.seal()));

    let ordinary = ordinary_take(16_000, 1, 4 * FRAMES + 1, 4_000);
    assert_eq!(
        take.len(),
        ordinary.len() + HISTORY_FRAMES,
        "96 ms replayed"
    );
    assert!(take.ends_with(&ordinary));
}

#[test]
fn a_take_starts_with_96_ms_before_the_press() {
    let fake = FakeMic::new(16_000, 1);
    let mic = open(&fake);
    arm(&mic);
    let before = 3_200;
    play(&fake, 1, 1, before);
    thread::sleep(SETTLE);
    post(&mic, recording(FRAMES).0);
    play(&fake, 1, before + 1, BLOCK_FRAMES);

    let take = samples(while_running(&fake, || mic.seal()));

    assert_eq!(
        take,
        ramp(
            before / FRAMES * FRAMES - HISTORY_FRAMES + 1,
            before + BLOCK_FRAMES
        )
    );
}

#[test]
fn quick_takes_share_no_audio() {
    let fake = FakeMic::new(16_000, 1);
    let mic = open(&fake);
    arm(&mic);
    let playing = AtomicBool::new(true);
    let takes: Vec<Vec<f32>> = thread::scope(|scope| {
        scope.spawn(|| {
            let mut next = 1;
            while playing.load(Ordering::Relaxed) {
                play(&fake, 1, next, BLOCK_FRAMES);
                next += BLOCK_FRAMES;
                thread::sleep(Duration::from_millis(10));
            }
        });
        let takes = (0..30)
            .map(|_| {
                let (take, ready) = recording(FRAMES);
                post(&mic, take);
                ready
                    .recv_timeout(Duration::from_secs(1))
                    .expect("audio reaches the take");
                thread::sleep(Duration::from_millis(30));
                let take = samples(mic.seal());
                mic.arm().expect("the microphone rearms");
                take
            })
            .collect();
        playing.store(false, Ordering::Relaxed);
        takes
    });

    for take in &takes {
        let (first, last) = (take[0] as usize, take[take.len() - 1] as usize);
        assert_eq!(take, &ramp(first, last), "a take loses nothing inside it");
    }
    for pair in takes.windows(2) {
        let (last, next) = (pair[0][pair[0].len() - 1] as usize, pair[1][0] as usize);
        assert!(next > last, "frame {next} is in two takes");
    }
}

#[test]
fn a_take_that_panics_keeps_its_audio_and_the_microphone_records_on() {
    let fake = FakeMic::new(16_000, 1);
    let mic = open(&fake);
    post(&mic, panicking_recording());
    arm(&mic);
    play(&fake, 1, 1, 2 * FRAMES);

    let sealed = while_running(&fake, || mic.seal());

    assert!(sink_panicked(&sealed));
    assert_eq!(samples(sealed), ramp(1, FRAMES), "up to the panic");
    assert!(!mic.needs_reopen());
    post(&mic, recording(FRAMES).0);
    arm(&mic);
    play(&fake, 1, 2 * FRAMES + 1, FRAMES);
    assert_eq!(
        samples(while_running(&fake, || mic.seal())),
        ramp(2 * FRAMES + 1, 3 * FRAMES)
    );
}

#[test]
fn a_take_that_panics_in_the_replay_still_comes_back() {
    let fake = FakeMic::new(16_000, 1);
    let mic = open(&fake);
    arm(&mic);
    play(&fake, 1, 1, 2 * FRAMES);
    thread::sleep(SETTLE);
    post(&mic, panicking_recording());
    play(&fake, 1, 2 * FRAMES + 1, FRAMES);

    let sealed = while_running(&fake, || mic.seal());

    assert!(sink_panicked(&sealed));
    assert_eq!(samples(sealed), ramp(1, FRAMES), "the first replayed chunk");
}

#[test]
fn the_device_stays_open_between_takes_until_the_microphone_drops() {
    let fake = FakeMic::new(16_000, 1);
    let mic = open(&fake);
    arm(&mic);

    let sealed = while_running(&fake, || mic.seal());

    assert!(sealed.recording.is_none());
    assert!(fake.is_streaming());
    drop(mic);
    assert!(!fake.is_streaming());
}

#[test]
fn dropping_a_listening_microphone_closes_the_device() {
    let fake = FakeMic::new(16_000, 1);
    let mic = open(&fake);
    arm(&mic);
    play(&fake, 1, 1, FRAMES);

    drop(mic);

    assert!(!fake.is_streaming());
}

#[test]
fn a_device_failure_mid_take_keeps_the_audio_before_it() {
    let fake = FakeMic::new(16_000, 1);
    let mic = open(&fake);
    let (take, ready) = recording(FRAMES);
    post(&mic, take);
    arm(&mic);
    play(&fake, 1, 1, FRAMES);
    ready
        .recv_timeout(Duration::from_secs(1))
        .expect("audio reaches the take");

    assert!(fake.report_error(FakeError::DeviceNotAvailable));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !mic.needs_reopen() {
        assert!(Instant::now() < deadline, "the failure is reported");
        thread::sleep(Duration::from_millis(1));
    }

    assert!(
        mic.arm().is_err(),
        "an armed microphone reports its failure"
    );
    let sealed = mic.seal();
    assert!(matches!(
        &sealed.end,
        Ok(end) if matches!(end.reason, EndReason::RecorderFailed(_))
    ));
    assert_eq!(samples(sealed), ramp(1, FRAMES));
}
