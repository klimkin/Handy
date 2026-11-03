#![cfg(test)]

use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use loom::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

macro_rules! info {
    ($($argument:tt)*) => { let _ = format_args!($($argument)*); };
}

struct Shared {
    cancel_epoch: AtomicU64,
    in_flight: Mutex<Option<InFlight>>,
}

struct EngineSupervisor {
    shared: Arc<Shared>,
}

struct Owner {
    shared: Arc<Shared>,
    worker: Option<Worker>,
}

#[derive(Clone, Copy, Debug)]
enum WorkerReply {
    HungUntilCancel,
    BufferedAfterCancel,
    Immediate,
}

struct Control {
    pid: u32,
    sent: AtomicBool,
    killed: AtomicBool,
    cancel_done: Mutex<bool>,
    cancel_completed: Condvar,
}

impl Control {
    fn send(&self, _frame: Vec<u8>) -> bool {
        assert!(
            !self.sent.swap(true, Ordering::SeqCst),
            "one request per model"
        );
        true
    }

    fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
    }

    fn finish_cancel(&self) {
        *self.cancel_done.lock().unwrap() = true;
        self.cancel_completed.notify_all();
    }

    fn await_cancel(&self) {
        let mut done = self.cancel_done.lock().unwrap();
        while !*done {
            done = self.cancel_completed.wait(done).unwrap();
        }
    }
}

struct Worker {
    control: Arc<Control>,
    reply: WorkerReply,
}

impl Worker {
    fn wait(&mut self, _request: &Request, _deadline: Duration) -> Result<Response, Failure> {
        match self.reply {
            WorkerReply::HungUntilCancel => {
                self.control.await_cancel();
                assert!(
                    self.control.killed.load(Ordering::SeqCst),
                    "cancel returned but marked work was neither rejected nor killed"
                );
                Err(Failure::Crashed)
            }
            WorkerReply::BufferedAfterCancel => {
                self.control.await_cancel();
                Ok(Response)
            }
            WorkerReply::Immediate => Ok(Response),
        }
    }

    fn died(&mut self) -> Failure {
        Failure::Crashed
    }
}

struct Request;

#[derive(Debug, PartialEq, Eq)]
struct Response;

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    Cancelled,
    Crashed,
}

impl Failure {
    fn worker_lost(&self) -> bool {
        *self == Self::Crashed
    }
}

fn encode_request(_request: &Request, _pcm: Option<&[f32]>) -> Result<Vec<u8>, Failure> {
    Ok(Vec::new())
}

fn request_name(_request: &Request) -> &'static str {
    "transcription"
}

include!(concat!(env!("OUT_DIR"), "/production.rs"));

fn setup(reply: WorkerReply) -> (Owner, EngineSupervisor, Arc<Control>) {
    let shared = Arc::new(Shared {
        cancel_epoch: AtomicU64::new(0),
        in_flight: Mutex::new(None),
    });
    let control = Arc::new(Control {
        pid: 1,
        sent: AtomicBool::new(false),
        killed: AtomicBool::new(false),
        cancel_done: Mutex::new(false),
        cancel_completed: Condvar::new(),
    });
    (
        Owner {
            shared: Arc::clone(&shared),
            worker: Some(Worker {
                control: Arc::clone(&control),
                reply,
            }),
        },
        EngineSupervisor { shared },
        control,
    )
}

fn exhaustive_builder() -> loom::model::Builder {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;
    builder.max_branches = 1_000;
    builder.max_permutations = None;
    builder.max_duration = None;
    builder.preemption_bound = None;
    builder.checkpoint_file = None;
    builder.expect_explicit_explore = false;
    builder
}

fn check_cancellation(reply: WorkerReply) {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc as HostArc;

    let schedules = HostArc::new(AtomicUsize::new(0));
    let outcomes = HostArc::new(AtomicUsize::new(0));
    let counted = HostArc::clone(&schedules);
    let covered = HostArc::clone(&outcomes);
    exhaustive_builder().check(move || {
        let (mut owner, engine, control) = setup(reply);
        let mark = engine.shared.epoch();
        let run =
            loom::thread::spawn(move || owner.call_work(&Request, None, Duration::ZERO, mark));
        let cancelled_control = Arc::clone(&control);
        let cancel = loom::thread::spawn(move || {
            engine.cancel();
            cancelled_control.finish_cancel();
        });
        let result = run.join().unwrap();
        cancel.join().unwrap();
        assert_eq!(
            result,
            Err(Failure::Cancelled),
            "cancelled call returned a successful reply"
        );
        assert!(
            control.sent.load(Ordering::SeqCst) || !control.killed.load(Ordering::SeqCst),
            "cancel killed an unpublished worker"
        );
        let path = if control.sent.load(Ordering::SeqCst) {
            2
        } else {
            1
        };
        covered.fetch_or(path, std::sync::atomic::Ordering::Relaxed);
        counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    assert_eq!(
        outcomes.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "check must explore both rejected and published work"
    );
    println!(
        "{reply:?}: {} exhaustive executions; rejected and published paths covered",
        schedules.load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[test]
fn cancel_stops_hung_marked_work() {
    check_cancellation(WorkerReply::HungUntilCancel);
}

#[test]
fn cancel_rejects_buffered_reply() {
    check_cancellation(WorkerReply::BufferedAfterCancel);
}

#[test]
fn fresh_mark_runs_after_cancel() {
    exhaustive_builder().check(|| {
        let (mut owner, engine, control) = setup(WorkerReply::Immediate);
        engine.cancel();
        let mark = engine.shared.epoch();
        assert_eq!(
            owner.call_work(&Request, None, Duration::ZERO, mark),
            Ok(Response)
        );
        assert!(control.sent.load(Ordering::SeqCst));
        assert!(!control.killed.load(Ordering::SeqCst));
    });
}
