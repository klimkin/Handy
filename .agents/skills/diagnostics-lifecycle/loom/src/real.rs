include!(concat!(env!("OUT_DIR"), "/real.rs"));

fn config() -> ConfigSnapshot {
    ConfigSnapshot {
        model: "whisper-small".into(),
        live_decode: "none".into(),
        paste_method: "None".into(),
        push_to_talk: false,
        post_process_enabled: false,
        vad: true,
    }
}

fn parse(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn events(trace: &serde_json::Value) -> Vec<&serde_json::Value> {
    trace["traceEvents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["ph"] != "M")
        .collect()
}

#[test]
fn stop_drains_admitted() {
    let mut cases = 0;
    for capacity in 1..=3 {
        for count in 0..=4 {
            let dir = tempfile::tempdir().unwrap();
            let recorder = Recorder::new();
            recorder
                .start_inner(dir.path(), config(), capacity, 1_000_000)
                .unwrap();
            for n in 0..count {
                recorder.instant(
                    None,
                    Name::Effect,
                    Args {
                        count: Some(n),
                        ..Args::default()
                    },
                );
            }
            let status = recorder.status();
            let admitted = status.events;
            assert_eq!(
                admitted + status.dropped,
                count,
                "emission accounting lost an input"
            );
            let trace = parse(&recorder.stop().unwrap());
            assert_eq!(
                events(&trace).len() as u64,
                admitted,
                "stop lost admitted event in real writer; queue={capacity} count={count}"
            );
            cases += 1;
        }
    }
    println!("EXHAUSTED real stop inputs: {cases}");
}

#[test]
fn stale_timestamp_rejected() {
    for offset in [-1_i32, 0, 1] {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new();
        recorder
            .start_inner(dir.path(), config(), 4, 1_000_000)
            .unwrap();
        let epoch = recorder.capture.read().unwrap().as_ref().unwrap().epoch;
        let at = if offset < 0 {
            epoch - Duration::from_secs(1)
        } else {
            epoch + Duration::from_secs(offset as u64)
        };
        recorder.instant_at(at, None, Name::Effect, Args::default());
        let status = recorder.status();
        assert_eq!(
            status.events,
            u64::from(offset >= 0),
            "pre-epoch event admitted into new capture"
        );
        recorder.stop().unwrap();
    }
    println!("EXHAUSTED epoch inputs: before, equal, after");
}

#[test]
fn old_span_isolation() {
    let mut failures = Vec::new();
    for begun_active in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new();
        if begun_active {
            recorder
                .start_inner(dir.path(), config(), 4, 1_000_000)
                .unwrap();
        }
        let span = recorder.span(recorder.next_run(), Name::Output);
        if begun_active {
            recorder.stop().unwrap();
        }
        recorder
            .start_inner(dir.path(), config(), 4, 1_000_000)
            .unwrap();
        span.end(Args::code(Code::Typed));
        let trace = parse(&recorder.stop().unwrap());
        if !events(&trace).is_empty() {
            failures.push(format!("span begins {} -> start new capture -> Span::end -> new trace contains old output end", if begun_active { "in capture A -> stop A" } else { "while inactive" }));
        }
    }
    for failure in &failures {
        println!("COUNTEREXAMPLE isolation: {failure}");
    }
    assert!(failures.is_empty(), "old span entered newer capture");
}

struct WriterOutput {
    bytes: u64,
    incomplete: u64,
    written: usize,
    thread_metadata: usize,
}

fn writer_case(sequence: &[u8], cap: u64) -> WriterOutput {
    let dir = tempfile::tempdir().unwrap();
    let partial = dir.path().join("capture.partial");
    let final_path = dir.path().join("capture.json");
    let file = File::create(&partial).unwrap();
    let (sender, rx) = sync_channel(4);
    let (done_tx, done_rx) = mpsc::channel();
    let epoch = Instant::now();
    for (index, phase) in sequence.iter().enumerate() {
        sender
            .try_send(Event {
                at: epoch,
                phase: match phase {
                    0 => Phase::Instant,
                    1 => Phase::Begin,
                    _ => Phase::End,
                },
                name: Name::Output,
                run: Some(RunId(NonZeroU64::new(index as u64 + 1).unwrap())),
                thread: ThreadTag {
                    tid: index as u64 + 1,
                    name: None,
                },
                args: Args::default(),
            })
            .unwrap_or_else(|_| panic!("fixture queue unexpectedly full"));
    }
    drop(sender);
    Writer {
        rx,
        done_tx,
        out: BufWriter::new(file),
        partial,
        final_path,
        epoch,
        byte_cap: cap,
        dropped: Arc::new(AtomicU64::new(0)),
        config: config(),
    }
    .run();
    let path = done_rx.recv().unwrap();
    let trace = parse(&path);
    let accepted: Vec<_> = events(&trace)
        .into_iter()
        .filter(|ev| ev["args"]["incomplete"] != true)
        .collect();
    let mut used = b"{\"traceEvents\":[\n".len() as u64;
    for (index, event) in accepted.iter().enumerate() {
        used += serde_json::to_vec(event).unwrap().len() as u64 + u64::from(index > 0) + 1;
    }
    assert!(
        accepted.is_empty() || used <= cap,
        "event byte budget bypassed; cap={cap}, accepted bytes={used}"
    );
    let written = accepted.len();
    WriterOutput {
        bytes: fs::metadata(&path).unwrap().len(),
        incomplete: trace["otherData"]["incomplete_spans"].as_u64().unwrap(),
        written,
        thread_metadata: trace["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["name"] == "thread_name")
            .count(),
    }
}

fn sequences() -> Vec<Vec<u8>> {
    let mut result = vec![Vec::new()];
    for length in 1..=3_u32 {
        for value in 0..3_usize.pow(length) {
            let mut number = value;
            let mut sequence = Vec::new();
            for _ in 0..length {
                sequence.push((number % 3) as u8);
                number /= 3;
            }
            result.push(sequence);
        }
    }
    result
}

#[test]
fn writer_drain_inputs() {
    for sequence in sequences() {
        let output = writer_case(&sequence, 1_000_000);
        assert_eq!(output.written, sequence.len(), "writer lost admitted event");
    }
    println!("EXHAUSTED writer drain inputs: 40");
}

#[test]
fn accepted_event_budget() {
    let mut cases = 0;
    for cap in [0, 1, 16, 64, 128, 256, 512, 1024] {
        for sequence in sequences() {
            writer_case(&sequence, cap);
            cases += 1;
        }
    }
    println!("EXHAUSTED accepted-byte inputs: {cases}");
}

#[test]
fn total_capture_resource_limit() {
    let mut failures = 0;
    let mut examples = Vec::new();
    let mut retained_threads = Vec::new();
    for cap in [0, 1, 16, 64, 128, 256, 512, 1024] {
        for sequence in sequences() {
            let output = writer_case(&sequence, cap);
            let WriterOutput {
                bytes,
                incomplete,
                written,
                thread_metadata,
            } = output;
            if cap == 0 && sequence.iter().all(|phase| *phase == 0) {
                assert_eq!(written, 0);
                retained_threads.push((sequence.len(), thread_metadata));
            }
            if bytes > cap {
                failures += 1;
                if examples.len() < 3 || (incomplete > 0 && examples.len() == 3) {
                    examples.push(format!("cap={cap}, phases={sequence:?} -> accepted={written}, synthetic ends={incomplete}, finished file={bytes} bytes"));
                }
            }
        }
    }
    for example in examples {
        println!("COUNTEREXAMPLE resource: {example}");
    }
    println!("COUNTEREXAMPLE memory: cap=0, accepted=0, (distinct producer tids, retained thread entries)={retained_threads:?}");
    println!("EXHAUSTED total-byte inputs: 320; violations={failures}");
    let thread_growth = retained_threads.iter().any(|(_, retained)| *retained != 0);
    assert!(failures == 0 && !thread_growth, "finished capture exceeds resource limits; byte violations={failures}, unbudgeted thread growth={thread_growth}");
}
