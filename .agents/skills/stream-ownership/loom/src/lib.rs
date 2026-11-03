#![cfg(test)]
#![allow(dead_code)]

use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

macro_rules! warn {
    ($($argument:tt)*) => {{ let _ = format_args!($($argument)*); }};
}
macro_rules! error {
    ($($argument:tt)*) => {{ let _ = format_args!($($argument)*); }};
}

fn exhaustive() -> loom::model::Builder {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 4;
    builder.max_branches = 1_000;
    builder.max_permutations = None;
    builder.max_duration = None;
    builder.preemption_bound = None;
    builder.checkpoint_file = None;
    builder.expect_explicit_explore = false;
    builder
}

#[derive(Debug, PartialEq, Eq)]
pub enum EngineError {
    Cancelled,
}

#[allow(clippy::useless_conversion, clippy::possible_missing_else)]
mod manager {
    use super::*;
    use loom::thread;
    use std::sync::mpsc;

    type RunId = u64;
    type RunOutcome = ();
    type Result<T> = std::result::Result<T, EngineError>;
    type StreamFinalizeReply = Result<Option<RunOutcome>>;
    type AudioFrameCallback = std::sync::Arc<dyn Fn(&[f32]) + Send + Sync>;
    struct CaptureSink;

    #[derive(Clone)]
    struct TranscriptionManager {
        router: Arc<StreamRouter>,
        active_stream_worker: Arc<AtomicU64>,
        next_stream_worker_id: Arc<AtomicU64>,
        stream_active: Arc<AtomicBool>,
        started: Arc<(Mutex<bool>, Condvar)>,
    }

    impl TranscriptionManager {
        fn run_stream_worker(
            &self,
            _rx: mpsc::Receiver<StreamCmd>,
            _cancelled: Arc<AtomicBool>,
            worker_id: u64,
            _run: RunId,
        ) {
            assert_eq!(self.active_stream_worker.load(Ordering::Acquire), worker_id);
            self.stream_active.store(true, Ordering::Release);
            let (started, changed) = &*self.started;
            *started.lock().unwrap() = true;
            changed.notify_all();
        }
    }

    include!(concat!(env!("OUT_DIR"), "/manager.rs"));

    #[test]
    fn guard_cannot_clear_replacement() {
        use std::sync::atomic::{AtomicUsize, Ordering as HostOrdering};
        use std::sync::Arc as HostArc;

        let schedules = HostArc::new(AtomicUsize::new(0));
        let replacements = HostArc::new(AtomicUsize::new(0));
        let counted = HostArc::clone(&schedules);
        let replaced = HostArc::clone(&replacements);
        exhaustive().check(move || {
            let manager = TranscriptionManager {
                router: Arc::new(StreamRouter::new()),
                active_stream_worker: Arc::new(AtomicU64::new(1)),
                next_stream_worker_id: Arc::new(AtomicU64::new(2)),
                stream_active: Arc::new(AtomicBool::new(true)),
                started: Arc::new((Mutex::new(false), Condvar::new())),
            };
            let old = StreamWorkerGuard {
                worker_id: 1,
                active_stream_worker: Arc::clone(&manager.active_stream_worker),
                stream_active: Arc::clone(&manager.stream_active),
            };
            let replacement = manager.clone();
            let start = thread::spawn(move || replacement.start_stream(2));
            drop(old);
            start.join().unwrap();
            let token = manager.active_stream_worker.load(Ordering::Acquire);
            if token == 2 {
                let (started, changed) = &*manager.started;
                let mut ready = started.lock().unwrap();
                while !*ready {
                    ready = changed.wait(ready).unwrap();
                }
                assert!(
                    manager.stream_active.load(Ordering::Acquire),
                    "PROPERTY guard: old cleanup cleared replacement worker state"
                );
                replaced.fetch_add(1, HostOrdering::Relaxed);
            } else {
                assert_eq!(token, 0, "PROPERTY guard: old cleanup must retire token 1");
            }
            counted.fetch_add(1, HostOrdering::Relaxed);
        });
        assert!(replacements.load(HostOrdering::Relaxed) > 0);
        println!(
            "guard: {} exhaustive executions; replacement started in {}",
            schedules.load(HostOrdering::Relaxed),
            replacements.load(HostOrdering::Relaxed)
        );
    }

    #[test]
    fn manager_preserves_cancelled_finalize() {
        exhaustive().check(|| {
            let manager = TranscriptionManager {
                router: Arc::new(StreamRouter::new()),
                active_stream_worker: Arc::new(AtomicU64::new(1)),
                next_stream_worker_id: Arc::new(AtomicU64::new(2)),
                stream_active: Arc::new(AtomicBool::new(true)),
                started: Arc::new((Mutex::new(false), Condvar::new())),
            };
            let (tx, rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let StreamCmd::Finalize { reply, .. } = rx.recv().unwrap() else {
                    panic!("expected finalize");
                };
                reply.send(Err(EngineError::Cancelled)).unwrap();
            });
            let result = manager.finalize_route(
                Route {
                    tx,
                    cancelled: Arc::new(AtomicBool::new(false)),
                },
                1,
            );
            worker.join().unwrap();
            assert_eq!(
                result,
                Err(EngineError::Cancelled),
                "PROPERTY manager-finalize: cancellation became batch fallback"
            );
        });
        println!("manager finalize: worker cancellation reply retained");
    }

    #[test]
    fn stalled_old_callback_cannot_feed_replacement() {
        exhaustive().check(|| {
            let router = Arc::new(StreamRouter::new());
            let (_old_rx, _) = router.open();
            let active = Arc::new(AtomicU64::new(1));
            let old_worker = StreamWorkerGuard {
                worker_id: 1,
                active_stream_worker: Arc::clone(&active),
                stream_active: Arc::new(AtomicBool::new(true)),
            };
            let stalled = Arc::new((Mutex::new((false, false)), Condvar::new()));
            let old_stalled = Arc::clone(&stalled);
            let callback_router = Arc::clone(&router);
            let old_callback = loom::thread::spawn(move || {
                let old_frame = [1.0];
                let on_audio: AudioFrameCallback = std::sync::Arc::new(move |frame| {
                    callback_router.feed(frame);
                });
                let (state, changed) = &*old_stalled;
                let mut state = state.lock().unwrap();
                state.0 = true;
                changed.notify_all();
                while !state.1 {
                    state = changed.wait(state).unwrap();
                }
                drop(state);
                CaptureSink::emit(&mut Vec::new(), &on_audio, &old_frame);
            });
            let (state, changed) = &*stalled;
            let mut entered = state.lock().unwrap();
            while !entered.0 {
                entered = changed.wait(entered).unwrap();
            }
            drop(entered);
            let old_route = router.take().unwrap();
            let _ = old_route.tx.send(StreamCmd::Cancel);
            drop(old_worker);
            assert_eq!(active.load(Ordering::Acquire), 0);
            let (replacement_rx, _) = router.open();
            state.lock().unwrap().1 = true;
            changed.notify_all();
            old_callback.join().unwrap();
            router.feed(&[2.0]);
            let frames: Vec<_> = replacement_rx.try_iter().filter_map(|command| match command {
                StreamCmd::Feed(frame) => Some(frame),
                _ => None,
            }).collect();
            println!("REPLAY router: old sink holds frame [1]; recorder stop times out; shutdown detaches delivery; take old route and cancel worker; token retires; open replacement; old CaptureSink::emit resumes into shared router; replacement receives {frames:?}");
            assert!(!frames.iter().any(|frame| frame == &[1.0]),
                "PROPERTY router: old recording frame reached replacement route");
            assert!(frames.iter().any(|frame| frame == &[2.0]),
                "PROPERTY router: replacement dropped its fresh frame");
        });
    }
}

mod engine {
    use super::*;
    use loom::sync::mpsc;

    struct Shared {
        cancel_epoch: AtomicU64,
    }

    struct ActiveStream {
        id: u64,
        epoch: u64,
        closed: Arc<AtomicBool>,
        pending_ms: i64,
        on_progress: Box<dyn FnMut(StreamProgress) + Send>,
    }

    struct Owner {
        shared: Arc<Shared>,
        stream: Option<ActiveStream>,
        worker: Option<()>,
        response: Boundary,
        dispatched: usize,
        resets: usize,
    }

    #[derive(Clone, Copy, Debug)]
    enum Boundary {
        Success,
        Cancelled,
        Remote,
        Lost,
        Unexpected,
    }

    enum Failure {
        Cancelled,
        Remote(String),
        Lost,
    }

    enum Request {
        Feed,
        Finalize { want_language: bool },
    }

    enum Command {
        Feed {
            id: u64,
            pcm: Vec<f32>,
        },
        Finalize {
            id: u64,
            want_language: bool,
            epoch: u64,
            reply: mpsc::Sender<Result<Option<Finalized>, EngineError>>,
        },
    }

    struct StreamUpdate;
    struct StreamText;

    struct StreamProgress {
        update: StreamUpdate,
        text: Option<StreamText>,
        elapsed: Duration,
    }

    pub struct Finalized {
        update: StreamUpdate,
        text: StreamText,
        language: Option<String>,
        elapsed: Duration,
    }

    enum Response {
        Fed {
            update: StreamUpdate,
            text: Option<StreamText>,
        },
        Finalized {
            update: StreamUpdate,
            text: StreamText,
            language: Option<String>,
        },
        Unexpected,
    }

    struct Deadlines;
    impl Deadlines {
        fn audio(&self, _samples: usize) -> Duration {
            Duration::ZERO
        }
    }

    fn ms_to_samples(_ms: i64) -> usize {
        0
    }
    fn pending_ms(_update: &StreamUpdate) -> i64 {
        0
    }
    fn unexpected(_response: &Response) -> &'static str {
        "unexpected"
    }

    impl Owner {
        fn deadlines(&self) -> Deadlines {
            Deadlines
        }
        fn reset_worker_stream(&mut self) {
            self.resets += 1;
        }
        fn worker_lost(&mut self, _what: &str, _failure: &Failure) {
            self.worker = None;
        }
        fn call_work(
            &mut self,
            request: &Request,
            _pcm: Option<&[f32]>,
            _deadline: Duration,
            _epoch: u64,
        ) -> Result<Response, Failure> {
            self.dispatched += 1;
            match self.response {
                Boundary::Success => match request {
                    Request::Feed => Ok(Response::Fed {
                        update: StreamUpdate,
                        text: None,
                    }),
                    Request::Finalize { .. } => Ok(Response::Finalized {
                        update: StreamUpdate,
                        text: StreamText,
                        language: None,
                    }),
                },
                Boundary::Cancelled => Err(Failure::Cancelled),
                Boundary::Remote => Err(Failure::Remote("remote".into())),
                Boundary::Lost => Err(Failure::Lost),
                Boundary::Unexpected => Ok(Response::Unexpected),
            }
        }
    }

    include!(concat!(env!("OUT_DIR"), "/engine.rs"));

    fn setup(
        stream_id: Option<u64>,
        worker: bool,
        cancelled: bool,
        closed: bool,
        response: Boundary,
    ) -> Owner {
        Owner {
            shared: Arc::new(Shared {
                cancel_epoch: AtomicU64::new(u64::from(cancelled)),
            }),
            stream: stream_id.map(|id| ActiveStream {
                id,
                epoch: 0,
                closed: Arc::new(AtomicBool::new(closed)),
                pending_ms: 0,
                on_progress: Box::new(|_| {}),
            }),
            worker: worker.then_some(()),
            response,
            dispatched: 0,
            resets: 0,
        }
    }

    #[test]
    fn frames_cannot_reach_replaced_stream() {
        exhaustive().check(|| {
            let mut cases = 0;
            for current in [None, Some(1), Some(2)] {
                for worker in [false, true] {
                    for cancelled in [false, true] {
                        for closed in [false, true] {
                            let mut owner = setup(current, worker, cancelled, closed, Boundary::Success);
                            let (commands, queue) = mpsc::channel();
                            let handle = StreamHandle {
                                id: 1, epoch: 0, closed: Arc::new(AtomicBool::new(false)),
                                commands, finished: false,
                            };
                            handle.feed(vec![0.0]);
                            let Command::Feed { id, pcm } = queue.recv().unwrap() else {
                                panic!("handle feed must preserve command kind");
                            };
                            owner.feed(id, &pcm);
                            let eligible = current == Some(1) && worker && !cancelled && !closed;
                            assert_eq!(owner.dispatched, usize::from(eligible),
                                "PROPERTY feed: frame crossed stream id/closed/cancel/worker fence; current={current:?} worker={worker} cancelled={cancelled} closed={closed}");
                            cases += 1;
                        }
                    }
                }
            }
            assert_eq!(cases, 24);
            println!("feed: {cases} exhaustive boundary states");
        });
    }

    #[test]
    fn cancelled_finalize_never_falls_back() {
        exhaustive().check(|| {
            let mut cases = 0;
            for current in [None, Some(1), Some(2)] {
                for worker in [false, true] {
                    let mut owner = setup(current, worker, true, false, Boundary::Success);
                    assert!(matches!(owner.finalize(1, false, 0), Err(EngineError::Cancelled)),
                        "PROPERTY finalize: observed cancel became batch fallback; current={current:?} worker={worker}");
                    assert_eq!(owner.dispatched, 0, "PROPERTY finalize: cancelled finalize dispatched work");
                    if current == Some(2) {
                        assert_eq!(owner.stream.as_ref().map(|s| s.id), Some(2),
                            "PROPERTY finalize: old cancelled finalize removed replacement stream");
                    }
                    cases += 1;
                }
            }
            let mut owner = setup(Some(1), true, false, false, Boundary::Cancelled);
            assert!(matches!(owner.finalize(1, false, 0), Err(EngineError::Cancelled)),
                "PROPERTY finalize-boundary: worker cancel became batch fallback");
            assert_eq!(owner.resets, 1, "PROPERTY finalize: cancelled worker stream was not reset");
            cases += 1;
            println!("finalize cancellation: {cases} exhaustive boundary states");
        });
    }

    #[test]
    fn handle_preserves_cancelled_finalize() {
        exhaustive().check(|| {
            let (commands, queue) = mpsc::channel();
            let handle = StreamHandle {
                id: 1,
                epoch: 0,
                closed: Arc::new(AtomicBool::new(false)),
                commands,
                finished: false,
            };
            let worker = loom::thread::spawn(move || {
                let mut owner = setup(None, false, true, false, Boundary::Success);
                let Command::Finalize {
                    id,
                    want_language,
                    epoch,
                    reply,
                } = queue.recv().unwrap()
                else {
                    panic!("handle finalize must preserve command kind");
                };
                let _ = reply.send(owner.finalize(id, want_language, epoch));
            });
            assert!(
                matches!(handle.finalize(false), Err(EngineError::Cancelled)),
                "PROPERTY handle-finalize: cancellation became batch fallback"
            );
            worker.join().unwrap();
        });
        println!("handle finalize: command epoch and owner cancellation retained");
    }

    #[test]
    fn uncancelled_finalize_retains_fallback() {
        exhaustive().check(|| {
            let mut cases = 0;
            for current in [None, Some(1), Some(2)] {
                for worker in [false, true] {
                    for response in [
                        Boundary::Success,
                        Boundary::Remote,
                        Boundary::Lost,
                        Boundary::Unexpected,
                    ] {
                        let mut owner = setup(current, worker, false, false, response);
                        let result = owner.finalize(1, false, 0);
                        let usable =
                            current == Some(1) && worker && matches!(response, Boundary::Success);
                        assert!(
                            matches!(result, Ok(Some(_))) == usable,
                            "PROPERTY fallback: uncancelled finalize returned wrong usable result"
                        );
                        assert!(
                            matches!(result, Ok(None)) != usable,
                            "PROPERTY fallback: uncancelled finalize returned wrong batch fallback"
                        );
                        cases += 1;
                    }
                }
            }
            assert_eq!(cases, 24);
            println!("uncancelled finalize: {cases} exhaustive boundary states");
        });
    }
}
