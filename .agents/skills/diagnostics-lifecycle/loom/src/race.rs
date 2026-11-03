use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use loom::sync::{Arc, Mutex, RwLock};
use serde::Serialize;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

include!(concat!(env!("OUT_DIR"), "/race.rs"));

struct Queue {
    events: Vec<Event>,
    drained: usize,
    closed: bool,
}

struct SyncSender<T> {
    queue: Arc<Mutex<Queue>>,
    marker: std::marker::PhantomData<T>,
}

impl SyncSender<Event> {
    fn try_send(&self, event: Event) -> Result<(), ()> {
        let mut queue = self.queue.lock().unwrap();
        if queue.closed || queue.events.len() == 2 {
            return Err(());
        }
        queue.events.push(event);
        Ok(())
    }
}

impl<T> Drop for SyncSender<T> {
    fn drop(&mut self) {
        self.queue.lock().unwrap().closed = true;
    }
}

mod mpsc {
    use super::*;

    pub struct Receiver<T> {
        pub queue: Arc<Mutex<Queue>>,
        pub marker: std::marker::PhantomData<T>,
    }

    impl Receiver<PathBuf> {
        pub fn recv_timeout(&self, _: Duration) -> Result<PathBuf, RecvTimeoutError> {
            let mut queue = self.queue.lock().unwrap();
            if !queue.closed {
                return Err(RecvTimeoutError::Timeout);
            }
            queue.drained = queue.events.len();
            queue.events.clear();
            Ok(PathBuf::from("drained"))
        }
    }
}

#[test]
fn stop_admission_interleavings() {
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = completed.clone();
    let admissions = std::sync::Arc::new(std::array::from_fn::<_, 3, _>(|_| {
        std::sync::atomic::AtomicUsize::new(0)
    }));
    let admission_counts = admissions.clone();
    let mut model = loom::model::Builder::new();
    model.max_threads = 3;
    model.max_branches = 1000;
    model.max_permutations = None;
    model.max_duration = None;
    model.preemption_bound = None;
    model.checkpoint_file = None;
    model.expect_explicit_explore = false;
    model.check(move || {
        let epoch = Instant::now();
        let recorder = Recorder::new();
        let queue = Arc::new(Mutex::new(Queue {
            events: Vec::new(),
            drained: 0,
            closed: false,
        }));
        let admitted = Arc::new(AtomicU64::new(0));
        *recorder.capture.write().unwrap() = Some(Capture {
            sender: SyncSender {
                queue: queue.clone(),
                marker: std::marker::PhantomData,
            },
            epoch,
            started_at_ms: 0,
            admitted: admitted.clone(),
            dropped: Arc::new(AtomicU64::new(0)),
        });
        *recorder.done_rx.lock().unwrap() = Some(mpsc::Receiver {
            queue: queue.clone(),
            marker: std::marker::PhantomData,
        });
        recorder.active.store(true, Ordering::Release);
        let recorder = Arc::new(recorder);
        let producer = recorder.clone();
        let emit = loom::thread::spawn(move || {
            for _ in 0..2 {
                producer.emit(Event {
                    at: epoch,
                    phase: Phase::Instant,
                    name: Name::Effect,
                    run: None,
                    thread: ThreadTag { tid: 1, name: None },
                    args: Args::default(),
                });
            }
        });
        let stopper = recorder.clone();
        let stop = loom::thread::spawn(move || stopper.stop());
        emit.join().unwrap();
        assert!(
            stop.join().unwrap().is_ok(),
            "stop did not finish after detaching capture"
        );
        let accepted = admitted.load(Ordering::Relaxed);
        assert_eq!(
            queue.lock().unwrap().drained as u64,
            accepted,
            "stop lost admitted event at detach boundary"
        );
        admission_counts[accepted as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    for count in 0..=2 {
        assert!(
            admissions[count].load(std::sync::atomic::Ordering::Relaxed) > 0,
            "missing schedule with {count} admitted events"
        );
    }
    println!(
        "EXHAUSTED stop admission schedules: {}",
        completed.load(std::sync::atomic::Ordering::Relaxed)
    );
}
