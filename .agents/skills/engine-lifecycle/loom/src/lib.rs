#![cfg(test)]
#![allow(dead_code)]

mod publication;

use loom::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::{
    collections::VecDeque,
    fmt, io,
    path::PathBuf,
    time::{Duration, Instant},
};

macro_rules! debug { ($($args:tt)*) => { let _ = format_args!($($args)*); }; }
macro_rules! info { ($($args:tt)*) => { let _ = format_args!($($args)*); }; }
macro_rules! warn { ($($args:tt)*) => { let _ = format_args!($($args)*); }; }

mod mpsc {
    use super::{Arc, Condvar, Mutex, VecDeque};
    pub use std::sync::mpsc::{RecvError, SendError};

    struct State<T> {
        queue: VecDeque<T>,
        senders: usize,
        receiver: bool,
    }
    struct Channel<T> {
        state: Mutex<State<T>>,
        changed: Condvar,
    }
    pub struct Sender<T>(Arc<Channel<T>>);
    pub struct Receiver<T>(Arc<Channel<T>>);

    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let channel = Arc::new(Channel {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                senders: 1,
                receiver: true,
            }),
            changed: Condvar::new(),
        });
        (Sender(Arc::clone(&channel)), Receiver(channel))
    }
    impl<T> Sender<T> {
        pub fn send(&self, value: T) -> Result<(), SendError<T>> {
            let mut state = self.0.state.lock().unwrap();
            if !state.receiver {
                return Err(SendError(value));
            }
            state.queue.push_back(value);
            self.0.changed.notify_all();
            Ok(())
        }
    }
    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            self.0.state.lock().unwrap().senders += 1;
            Self(Arc::clone(&self.0))
        }
    }
    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            self.0.state.lock().unwrap().senders -= 1;
            self.0.changed.notify_all();
        }
    }
    impl<T> Receiver<T> {
        pub fn recv(&self) -> Result<T, RecvError> {
            let mut state = self.0.state.lock().unwrap();
            while state.queue.is_empty() && state.senders != 0 {
                state = self.0.changed.wait(state).unwrap();
            }
            state.queue.pop_front().ok_or(RecvError)
        }
    }
    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            self.0.state.lock().unwrap().receiver = false;
            self.0.changed.notify_all();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Gpu,
    Cpu,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelector {
    Auto,
    Key(String),
    Index(usize),
}
#[derive(Debug, Clone)]
pub struct LoadedInfo {
    on_gpu: bool,
    backend: &'static str,
    device: &'static str,
}
#[derive(Clone, Debug)]
struct RunOptions;
struct StreamOptions;
struct ActiveStream {
    id: u64,
}
struct Finalized;
type Transcript = usize;
type Reply<T> = mpsc::Sender<T>;

#[derive(Debug)]
enum Request {
    Load {
        path: PathBuf,
        backend: Backend,
        device: DeviceSelector,
    },
    Run {
        options: RunOptions,
    },
}
#[derive(Debug)]
enum Response {
    Loaded(LoadedInfo),
    Transcript(Transcript),
    Other,
}

#[derive(Clone, Copy, Debug)]
enum Native {
    Ok,
    Crash,
    Hang,
    Remote,
    Cancel,
}
impl Native {
    fn failure(self) -> Failure {
        match self {
            Self::Ok => panic!("success is not failure"),
            Self::Crash => Failure::Crashed("fixture".into()),
            Self::Hang => Failure::Hung(Duration::ZERO),
            Self::Remote => Failure::Remote("fixture".into()),
            Self::Cancel => Failure::Cancelled,
        }
    }
}

#[derive(Default)]
struct Registry {
    records: Vec<Record>,
    loads: VecDeque<Native>,
    runs: VecDeque<Native>,
    run_calls: usize,
    cooperative: bool,
}
#[derive(Clone)]
struct Record {
    alive: bool,
    reaped: bool,
    cpu_only: bool,
}
struct Shared {
    gpu_allowed: bool,
    gpu_unavailable: AtomicBool,
    cancel_epoch: AtomicU64,
    loaded: Mutex<Option<LoadedInfo>>,
    unloads_requested: AtomicU64,
    registry: Arc<Mutex<Registry>>,
}
impl Shared {
    fn hello(&self, cpu_only: bool, _list_devices: bool) -> Result<Worker, Failure> {
        let ready = lock(&self.registry)
            .records
            .iter()
            .all(|record| !record.alive && record.reaped);
        assert!(
            ready,
            "replacement started before previous worker exited and was reaped"
        );
        let mut registry = lock(&self.registry);
        let id = registry.records.len();
        registry.records.push(Record {
            alive: true,
            reaped: false,
            cpu_only,
        });
        let cooperative = registry.cooperative;
        drop(registry);
        LIVE_WORKERS.fetch_add(1, Ordering::SeqCst);
        Ok(Worker {
            control: Arc::new(Control {
                pid: id as u32,
                child: Mutex::new(Child {
                    id,
                    registry: Arc::clone(&self.registry),
                    cooperative,
                }),
            }),
            info: None,
            cpu_only,
            registry: Arc::clone(&self.registry),
        })
    }
}
struct EngineSupervisor {
    commands: mpsc::Sender<Command>,
    shared: Arc<Shared>,
}
struct Control {
    pid: u32,
    child: Mutex<Child>,
}
impl Control {
    fn close_stdin(&self) {}
}
struct Child {
    id: usize,
    registry: Arc<Mutex<Registry>>,
    cooperative: bool,
}
impl Child {
    fn kill(&mut self) -> Result<(), io::Error> {
        lock(&self.registry).records[self.id].alive = false;
        Ok(())
    }
    fn wait(&mut self) -> Result<(), io::Error> {
        let mut registry = lock(&self.registry);
        registry.records[self.id].alive = false;
        registry.records[self.id].reaped = true;
        Ok(())
    }
}
fn wait_exit(child: &mut Child, _timeout: Duration) -> Option<()> {
    child.cooperative.then(|| {
        child.wait().unwrap();
    })
}
struct Worker {
    control: Arc<Control>,
    info: Option<LoadedInfo>,
    cpu_only: bool,
    registry: Arc<Mutex<Registry>>,
}
impl Worker {
    fn call(
        &mut self,
        request: &Request,
        _pcm: Option<&[f32]>,
        _deadline: Duration,
    ) -> Result<Response, Failure> {
        let outcome = lock(&self.registry).loads.pop_front().unwrap_or(Native::Ok);
        if !matches!(outcome, Native::Ok) {
            return Err(outcome.failure());
        }
        let Request::Load { backend, .. } = request else {
            unreachable!()
        };
        let on_gpu = !self.cpu_only && *backend != Backend::Cpu;
        let info = LoadedInfo {
            on_gpu,
            backend: if on_gpu { "Vulkan" } else { "CPU" },
            device: "fixture",
        };
        self.info = Some(info.clone());
        Ok(Response::Loaded(info))
    }
}
loom::lazy_static! { static ref LIVE_WORKERS: AtomicUsize = AtomicUsize::new(0); }

impl Owner {
    fn end_stream(&mut self) {
        self.stream = None;
    }
    fn stream_begin(
        &mut self,
        _active: ActiveStream,
        _run: &RunOptions,
        _stream: &StreamOptions,
    ) -> Result<(), EngineError> {
        unreachable!("no stream commands in engine lifecycle model")
    }
    fn feed(&mut self, _id: u64, _pcm: &[f32]) {
        unreachable!()
    }
    fn finalize(
        &mut self,
        _id: u64,
        _want_language: bool,
        _epoch: u64,
    ) -> Result<Option<Finalized>, EngineError> {
        unreachable!()
    }
    fn call_work(
        &mut self,
        _request: &Request,
        _pcm: Option<&[f32]>,
        _deadline: Duration,
        _epoch: u64,
    ) -> Result<Response, Failure> {
        let (calls, outcome) = {
            let mut registry = lock(&self.shared.registry);
            registry.run_calls += 1;
            (
                registry.run_calls,
                registry.runs.pop_front().unwrap_or(Native::Crash),
            )
        };
        assert!(calls <= 2, "transcription retry exceeded two attempts");
        if matches!(outcome, Native::Ok) {
            return Ok(Response::Transcript(1));
        }
        if matches!(outcome, Native::Cancel) {
            self.shared.cancel_epoch.fetch_add(1, Ordering::SeqCst);
        }
        Err(outcome.failure())
    }
}

include!(concat!(env!("OUT_DIR"), "/production.rs"));

fn builder() -> loom::model::Builder {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;
    builder.max_branches = 10_000;
    builder.max_permutations = None;
    builder.max_duration = None;
    builder.preemption_bound = None;
    builder.checkpoint_file = None;
    builder.expect_explicit_explore = false;
    builder
}
fn setup(cooperative: bool, gpu_allowed: bool) -> (Owner, EngineSupervisor) {
    let shared = Arc::new(Shared {
        gpu_allowed,
        gpu_unavailable: AtomicBool::new(false),
        cancel_epoch: AtomicU64::new(0),
        loaded: Mutex::new(None),
        unloads_requested: AtomicU64::new(0),
        registry: Arc::new(Mutex::new(Registry {
            cooperative,
            ..Registry::default()
        })),
    });
    let (commands, _) = mpsc::channel();
    (
        Owner {
            shared: Arc::clone(&shared),
            worker: None,
            spec: None,
            stream: None,
            unloads_done: 0,
        },
        EngineSupervisor { commands, shared },
    )
}
fn spec(backend: Backend, pinned: bool) -> LoadSpec {
    LoadSpec {
        path: "fixture.gguf".into(),
        backend,
        device: if pinned {
            DeviceSelector::Index(1)
        } else {
            DeviceSelector::Auto
        },
    }
}
fn assert_reaped(shared: &Shared) {
    let reaped = lock(&shared.registry)
        .records
        .iter()
        .all(|record| !record.alive && record.reaped);
    assert!(reaped, "unload acknowledged before worker was reaped");
}

#[test]
fn previous_worker_exits_before_replacement() {
    for cooperative in [false, true] {
        for word in 0u8..64 {
            builder().check(move || {
                let (mut owner, engine) = setup(cooperative, true);
                for bit in 0..6 {
                    if word & (1 << bit) != 0 {
                        owner.load(spec(Backend::Auto, false)).unwrap();
                    } else {
                        owner.unload();
                        assert_reaped(&engine.shared);
                    }
                }
                owner.unload();
                assert_reaped(&engine.shared);
            });
        }
    }
    println!("replacement: 128 lifecycle words, six load/unload operations each");
}

#[test]
fn unload_acknowledgement_follows_reaping() {
    for cooperative in [false, true] {
        builder().check(move || {
            let (mut owner, mut engine) = setup(cooperative, true);
            owner.load(spec(Backend::Auto, false)).unwrap();
            let (commands, queue) = mpsc::channel();
            engine.commands = commands;
            let worker = loom::thread::spawn(move || owner.run(queue));
            engine.unload().wait();
            assert_reaped(&engine.shared);
            drop(engine);
            worker.join().unwrap();
        });
    }
}

#[test]
fn pending_unload_blocks_late_publication() {
    builder().check(|| {
        let (owner, engine) = setup(true, true);
        let shared = Arc::clone(&engine.shared);
        let publish = loom::thread::spawn(move || {
            owner.publish_loaded(&LoadedInfo {
                on_gpu: true,
                backend: "Vulkan",
                device: "fixture",
            })
        });
        let (commands, _queue) = mpsc::channel();
        let mut engine = engine;
        engine.commands = commands;
        let _pending = engine.unload();
        publish.join().unwrap();
        assert_eq!(shared.unloads_requested.load(Ordering::SeqCst), 1);
        assert!(
            lock(&shared.loaded).is_none(),
            "pending unload resurrected loaded model"
        );
    });
}

#[test]
fn load_recovery_is_bounded_and_respects_selection() {
    let outcomes = [
        Native::Ok,
        Native::Crash,
        Native::Hang,
        Native::Remote,
        Native::Cancel,
    ];
    for first in outcomes {
        for second in outcomes {
            for backend in [Backend::Auto, Backend::Gpu, Backend::Cpu] {
                for pinned in [false, true] {
                    for gpu_allowed in [false, true] {
                        builder().check(move || {
                            let (mut owner, engine) = setup(true, gpu_allowed);
                            lock(&engine.shared.registry).loads = [first, second].into();
                            let result = owner.load(spec(backend, pinned));
                            let records = lock(&engine.shared.registry).records.clone();
                            assert!(records.len() <= 2, "model load retry exceeded two attempts");
                            if pinned || backend == Backend::Cpu || !gpu_allowed {
                                assert!(
                                    records.len() <= 1,
                                    "explicit selection silently fell back"
                                );
                            }
                            if records.len() == 2 {
                                assert!(
                                    records[1].cpu_only,
                                    "GPU recovery did not use CPU-only worker"
                                );
                            }
                            if !gpu_allowed || backend == Backend::Cpu {
                                assert!(
                                    records.iter().all(|record| record.cpu_only),
                                    "CPU choice registered GPU backend"
                                );
                            }
                            let retries = matches!(first, Native::Crash | Native::Hang)
                                && backend != Backend::Cpu
                                && !pinned
                                && gpu_allowed;
                            assert_eq!(
                                records.len(),
                                if retries { 2 } else { 1 },
                                "load recovery skipped or added an attempt"
                            );
                            let succeeds = matches!(first, Native::Ok)
                                || (retries && matches!(second, Native::Ok));
                            assert_eq!(
                                result.is_ok(),
                                succeeds,
                                "load recovery lost its native result"
                            );
                            owner.unload();
                            assert_reaped(&engine.shared);
                        });
                    }
                }
            }
        }
    }
    println!("load recovery: 300 outcome/selection/host combinations");
}

#[test]
fn transcription_retry_is_bounded() {
    let outcomes = [
        Native::Ok,
        Native::Crash,
        Native::Hang,
        Native::Remote,
        Native::Cancel,
    ];
    for first in outcomes {
        for second in outcomes {
            for backend in [Backend::Auto, Backend::Gpu, Backend::Cpu] {
                for pinned in [false, true] {
                    builder().check(move || {
                        let (mut owner, engine) = setup(true, true);
                        owner.load(spec(backend, pinned)).unwrap();
                        lock(&engine.shared.registry).runs = [first, second, Native::Crash].into();
                        let result = owner.transcribe(&[], &RunOptions, 0);
                        let calls = lock(&engine.shared.registry).run_calls;
                        assert!(calls <= 2, "transcription retry exceeded two attempts");
                        if pinned
                            || matches!(first, Native::Remote | Native::Cancel | Native::Ok)
                            || (backend == Backend::Cpu && matches!(first, Native::Hang))
                        {
                            assert_eq!(calls, 1, "failure bypassed no-retry policy");
                        }
                        if matches!(first, Native::Cancel) {
                            assert!(
                                matches!(result, Err(EngineError::Cancelled)),
                                "cancelled transcription lost cancellation"
                            );
                            assert!(
                                !engine.shared.gpu_unavailable.load(Ordering::Acquire),
                                "cancellation entered GPU recovery"
                            );
                        }
                        owner.unload();
                        assert_reaped(&engine.shared);
                    });
                }
            }
        }
    }
    println!("transcription recovery: 150 outcome/selection combinations");
}

#[test]
fn cancelled_queued_work_never_starts() {
    builder().check(|| {
        let (mut owner, engine) = setup(true, true);
        owner.load(spec(Backend::Auto, false)).unwrap();
        engine.shared.cancel_epoch.store(1, Ordering::SeqCst);
        let result = owner.transcribe(&[], &RunOptions, 0);
        assert_eq!(
            lock(&engine.shared.registry).run_calls,
            0,
            "old queued work started after cancel"
        );
        assert!(matches!(result, Err(EngineError::Cancelled)));
        owner.unload();
    });
}

#[test]
fn persisted_gpu_choice_never_falls_back() {
    for failure in [Native::Crash, Native::Hang] {
        builder().check(move || {
            let (mut owner, engine) = setup(true, true);
            lock(&engine.shared.registry).loads = [failure, Native::Ok].into();
            let selected = LoadSpec {
                path: "fixture.gguf".into(),
                backend: Backend::Auto,
                device: DeviceSelector::Key("selected-gpu".into()),
            };
            let result = owner.load(selected);
            let fell_back = result.as_ref().is_ok_and(|info| !info.on_gpu);
            owner.unload();
            assert!(
                !fell_back,
                "persisted explicit GPU selection silently fell back to CPU"
            );
        });
    }
}
