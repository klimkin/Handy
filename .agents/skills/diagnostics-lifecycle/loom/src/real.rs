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
    trace: serde_json::Value,
}

fn writer_events(
    input: &[Event],
    epoch: Instant,
    cap: u64,
    snapshot: ConfigSnapshot,
    dropped: u64,
) -> Result<Option<WriterOutput>, String> {
    let dir = tempfile::tempdir().unwrap();
    let partial = dir.path().join("capture.partial");
    let final_path = dir.path().join("capture.json");
    let file = File::create(&partial).unwrap();
    let (sender, rx) = sync_channel(input.len().max(1));
    let (done_tx, done_rx) = mpsc::channel();
    for event in input {
        sender
            .try_send(*event)
            .unwrap_or_else(|_| panic!("fixture queue unexpectedly full"));
    }
    drop(sender);
    Writer {
        rx,
        done_tx,
        out: BufWriter::new(file),
        partial: partial.clone(),
        final_path: final_path.clone(),
        epoch,
        byte_cap: cap,
        dropped: Arc::new(AtomicU64::new(dropped)),
        config: snapshot.clone(),
    }
    .run();
    let path = match done_rx.recv() {
        Ok(path) => path,
        Err(_) => {
            let recorder = Recorder::new();
            let capture_dir = dir.path().join("recorder-admission");
            if recorder.start_inner(&capture_dir, snapshot, 4, cap).is_ok() {
                let _ = recorder.stop();
                return Err(format!("writer rejected supported cap={cap}"));
            }
            if recorder.status().active
                || recorder.capture.read().unwrap().is_some()
                || recorder.done_rx.lock().unwrap().is_some()
                || capture_dir.exists()
                || partial.exists()
                || final_path.exists()
            {
                return Err(format!("unsupported cap={cap} left capture state or files"));
            }
            return Ok(None);
        }
    };
    let bytes = fs::metadata(&path).unwrap().len();
    let trace: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap())
        .map_err(|error| format!("cap={cap} produced invalid JSON: {error}"))?;
    let accepted: Vec<_> = events(&trace)
        .into_iter()
        .filter(|event| event["args"]["incomplete"] != true)
        .collect();
    let written = accepted.len();
    let truncated = trace["otherData"]["truncated_events"]
        .as_u64()
        .ok_or_else(|| "missing truncation count".to_string())?;
    if bytes > cap {
        return Err(format!(
            "cap={cap}, accepted={written}, finished file={bytes} bytes"
        ));
    }
    if written as u64 + truncated != input.len() as u64 {
        return Err(format!(
            "cap={cap} lost input accounting: written={written}, truncated={truncated}, input={}",
            input.len()
        ));
    }
    if trace["otherData"]["dropped_events"].as_u64() != Some(dropped) {
        return Err(format!("cap={cap} lost dropped counter"));
    }
    let expected_threads: std::collections::BTreeSet<_> = accepted
        .iter()
        .map(|event| event["tid"].as_u64().unwrap())
        .collect();
    let metadata: Vec<_> = trace["traceEvents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["name"] == "thread_name")
        .map(|event| event["tid"].as_u64().unwrap())
        .collect();
    let actual_threads: std::collections::BTreeSet<_> = metadata.iter().copied().collect();
    if actual_threads != expected_threads || metadata.len() != actual_threads.len() {
        return Err(format!(
            "cap={cap} retained rejected or duplicate thread metadata: expected={}, actual={}",
            expected_threads.len(),
            metadata.len()
        ));
    }
    let mut open = BTreeMap::new();
    for event in accepted {
        let Some(run) = event["id"].as_u64() else {
            continue;
        };
        let key = (run, event["name"].as_str().unwrap().to_string());
        match event["ph"].as_str().unwrap() {
            "b" => {
                open.insert(key, event["tid"].as_u64().unwrap());
            }
            "e" => {
                open.remove(&key);
            }
            _ => {}
        }
    }
    let incomplete = trace["otherData"]["incomplete_spans"].as_u64().unwrap();
    let closures: Vec<_> = events(&trace)
        .into_iter()
        .filter(|event| event["args"]["incomplete"] == true)
        .collect();
    if incomplete != open.len() as u64 || closures.len() as u64 != incomplete {
        return Err(format!("cap={cap} did not close every accepted begin"));
    }
    for event in closures {
        let key = (
            event["id"].as_u64().unwrap(),
            event["name"].as_str().unwrap().to_string(),
        );
        if event["ph"] != "e" || open.remove(&key) != event["tid"].as_u64() {
            return Err(format!("cap={cap} emitted incorrect incomplete closure"));
        }
    }
    Ok(Some(WriterOutput {
        bytes,
        incomplete,
        written,
        trace,
    }))
}

fn writer_case(sequence: &[u8], cap: u64) -> Result<Option<WriterOutput>, String> {
    let epoch = Instant::now();
    let input: Vec<_> = sequence
        .iter()
        .enumerate()
        .map(|(index, phase)| Event {
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
        .collect();
    writer_events(&input, epoch, cap, config(), 0)
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
        let output = writer_case(&sequence, 1_000_000)
            .unwrap_or_else(|error| panic!("writer lost admitted event; {error}"))
            .expect("ample cap rejected");
        assert_eq!(output.written, sequence.len(), "writer lost admitted event");
    }
    println!("EXHAUSTED writer drain inputs: 40");
}

#[test]
fn accepted_event_budget() {
    let mut cases = 0;
    let mut failures = Vec::new();
    for cap in [0, 1, 16, 64, 128, 256, 512, 1024] {
        for sequence in sequences() {
            if let Err(error) = writer_case(&sequence, cap) {
                failures.push(error);
            }
            cases += 1;
        }
    }
    println!("EXHAUSTED accepted-byte inputs: {cases}");
    assert!(
        failures.is_empty(),
        "event byte budget bypassed; {} invalid outputs; first={:?}",
        failures.len(),
        failures.first()
    );
}

#[test]
fn total_capture_resource_limit() {
    let mut cases = 0;
    let mut failures = Vec::new();
    for cap in [0, 1, 16, 64, 128, 256, 512, 1024] {
        for sequence in sequences() {
            if let Err(error) = writer_case(&sequence, cap) {
                failures.push(error);
            }
            cases += 1;
        }
    }
    println!(
        "EXHAUSTED total-byte inputs: {cases}; violations={}",
        failures.len()
    );

    let epoch = Instant::now();
    let maximal = |phase, offset| Event {
        at: epoch + Duration::from_secs(123_456),
        phase,
        name: Name::FirstAudioDelivered,
        run: Some(RunId(NonZeroU64::new(u64::MAX - offset).unwrap())),
        thread: ThreadTag {
            tid: u64::MAX - offset,
            name: None,
        },
        args: Args {
            code: Some(Code::MicrophonePermissionDenied),
            count: Some(u64::MAX),
            flag: Some(false),
        },
    };
    let mut producer_cases = 0;
    for count in [1, 2, 64, 512] {
        let input: Vec<_> = (0..count)
            .map(|offset| maximal(Phase::Begin, offset))
            .collect();
        for cap in [2048, 4096, 8192] {
            match writer_events(&input, epoch, cap, config(), u64::MAX) {
                Ok(Some(output)) => {
                    assert!(output.bytes <= cap);
                    assert_eq!(output.incomplete as usize, output.written);
                    if count == 512 {
                        assert!(
                            output.written < input.len(),
                            "fixture did not truncate producers"
                        );
                    }
                }
                Ok(None) => failures.push(format!(
                    "adversarial fixture cap={cap} unexpectedly unsupported"
                )),
                Err(error) => failures.push(error),
            }
            producer_cases += 1;
        }
    }
    println!("EXHAUSTED maximal-counter distinct-producer inputs: {producer_cases}");

    let mut closure_cases = 0;
    let mut saw_incomplete = false;
    let mut saw_complete = false;
    for cap in (512..=4096).step_by(17) {
        let begin = maximal(Phase::Begin, 0);
        let mut end = maximal(Phase::End, 0);
        end.at += Duration::from_secs(1);
        for input in [&[begin][..], &[begin, end][..]] {
            match writer_events(input, epoch, cap, config(), u64::MAX) {
                Ok(Some(output)) => {
                    saw_incomplete |= output.incomplete > 0;
                    saw_complete |= output.written == 2 && output.incomplete == 0;
                }
                Ok(None) => {}
                Err(error) => failures.push(error),
            }
            closure_cases += 1;
        }
    }
    println!("EXHAUSTED closure-budget inputs: {closure_cases}");
    assert!(
        !failures.is_empty() || (saw_incomplete && saw_complete),
        "closure fixtures missed accepted spans"
    );

    let begin = Event {
        at: epoch,
        phase: Phase::Begin,
        name: Name::Output,
        run: Some(RunId(NonZeroU64::new(1).unwrap())),
        thread: ThreadTag { tid: 1, name: None },
        args: Args::default(),
    };
    let replacement = Event {
        thread: ThreadTag {
            tid: u64::MAX,
            name: Some("handy-recorder-delivery"),
        },
        ..begin
    };
    let end = Event {
        phase: Phase::End,
        ..replacement
    };
    let large = maximal(Phase::Instant, 0);
    let small = Event {
        phase: Phase::Instant,
        run: None,
        ..begin
    };
    let rich_end = Event {
        phase: Phase::End,
        args: large.args,
        ..end
    };
    let inputs = [
        vec![begin, replacement],
        vec![begin, replacement, end],
        vec![begin, rich_end],
        vec![begin, large, small],
    ];
    let mut transitions = 0;
    let mut saw_replacement = false;
    let mut saw_rejected_end = false;
    let mut saw_later_small = false;
    for cap in (512..=2048).step_by(7) {
        for (case, input) in inputs.iter().enumerate() {
            match writer_events(input, epoch, cap, config(), 0) {
                Ok(Some(output)) => {
                    let actual = events(&output.trace);
                    let accepted: Vec<_> = actual
                        .iter()
                        .filter(|event| event["args"]["incomplete"] != true)
                        .collect();
                    if case == 0 && output.written == 2 {
                        saw_replacement = true;
                        assert_eq!(actual.last().unwrap()["tid"], u64::MAX);
                    }
                    saw_rejected_end |= case == 2 && output.written == 1 && output.incomplete == 1;
                    saw_later_small |= case == 3 && output.written == 2 && accepted[1]["tid"] == 1;
                }
                Ok(None) => {}
                Err(error) => failures.push(error),
            }
            transitions += 1;
        }
    }
    assert!(
        !failures.is_empty() || (saw_replacement && saw_rejected_end && saw_later_small),
        "transition fixtures missed replacement, rejected end, or later small event"
    );
    println!("EXHAUSTED reservation transition inputs: {transitions}");

    let mut escaped = config();
    escaped.model = "\\\"\n".repeat(2048);
    for cap in [0, 512, 4096, 32_768] {
        match writer_events(&[begin, end], epoch, cap, escaped.clone(), u64::MAX) {
            Ok(Some(output)) => {
                assert_eq!(output.trace["otherData"]["config"]["model"], escaped.model);
                assert_eq!(output.written, 2);
            }
            Ok(None) if cap < 32_768 => {}
            Ok(None) => failures.push("escaped config rejected with ample cap".into()),
            Err(error) => failures.push(error),
        }
    }
    println!("EXHAUSTED escaped-config inputs: 4");
    for failure in failures.iter().take(5) {
        println!("COUNTEREXAMPLE resource: {failure}");
    }
    assert!(
        failures.is_empty(),
        "finished capture exceeds resource limits; violations={}",
        failures.len()
    );
}
