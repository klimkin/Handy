use super::*;
use crate::language_detection::LanguageSource;
use crate::settings::get_default_settings;

fn config(model: &str, accelerator: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("model".to_string(), model.to_string()),
        ("accelerator".to_string(), accelerator.to_string()),
        ("selected_language".to_string(), "en".to_string()),
    ])
}

fn run(model: &str, result: RunResult, exit_ms: f64) -> RunRecord {
    RunRecord {
        config: config(model, "gpu"),
        language: Some("en".to_string()),
        entry_ms: Some(50.0),
        release_to_paste_ms: exit_ms,
        post_process_ms: None,
        result,
        speech_detected: true,
    }
}

fn manager() -> (tempfile::TempDir, StatsManager) {
    let dir = tempfile::tempdir().unwrap();
    let manager = StatsManager::at(dir.path().join("stats.db"));
    (dir, manager)
}

#[test]
fn errors_and_empty_speech_fail_but_empty_without_speech_says_nothing() {
    let (_dir, stats) = manager();
    for result in [RunResult::Text, RunResult::Error, RunResult::Empty] {
        stats.try_record_run(&run("m", result, 400.0)).unwrap();
    }
    stats
        .try_record_run(&RunRecord {
            speech_detected: false,
            ..run("m", RunResult::Empty, 300.0)
        })
        .unwrap();
    let summary = stats.summary(BTreeMap::new()).unwrap();
    assert_eq!((summary.groups[0].runs, summary.groups[0].failures), (3, 2));
}

#[test]
fn entry_includes_captured_failures_but_exit_only_successful_processing() {
    let (_dir, stats) = manager();
    stats
        .try_record_run(&run("m", RunResult::Text, 400.0))
        .unwrap();
    stats
        .try_record_run(&RunRecord {
            entry_ms: Some(100.0),
            ..run("m", RunResult::Error, 9_000.0)
        })
        .unwrap();
    let summary = stats.summary(BTreeMap::new()).unwrap();
    let group = &summary.groups[0];
    let entry = group.entry_ms.unwrap();
    let exit = group.exit_ms.unwrap();
    assert_eq!((entry.min, entry.max), (50.0, 100.0));
    assert_eq!((exit.min, exit.max), (400.0, 400.0));
}

#[test]
fn missing_capture_and_failed_processing_do_not_invent_zero_timings() {
    let (_dir, stats) = manager();
    stats
        .try_record_run(&RunRecord {
            entry_ms: None,
            ..run("m", RunResult::Error, 50.0)
        })
        .unwrap();
    let summary = stats.summary(BTreeMap::new()).unwrap();
    let group = &summary.groups[0];
    assert_eq!((group.runs, group.failures), (1, 1));
    assert!(group.entry_ms.is_none() && group.exit_ms.is_none());
}

#[test]
fn language_and_settings_make_separate_groups_and_variants_follow_first_run() {
    let stored = |record: RunRecord, timestamp| StoredRun {
        exit_ms: Some(record.release_to_paste_ms - record.post_process_ms.unwrap_or(0.0)),
        record,
        timestamp,
    };
    let summary = summarize(
        vec![
            stored(run("m", RunResult::Text, 400.0), 10),
            stored(
                RunRecord {
                    config: config("m", "cpu"),
                    ..run("m", RunResult::Text, 600.0)
                },
                10,
            ),
            stored(
                RunRecord {
                    language: Some("de".into()),
                    ..run("m", RunResult::Text, 500.0)
                },
                30,
            ),
            stored(run("m", RunResult::Text, 800.0), 40),
        ],
        config("m", "cpu"),
    );
    assert_eq!(summary.groups.len(), 3);
    let gpu = &summary.groups[0];
    let cpu = &summary.groups[1];
    assert_eq!((gpu.variant, cpu.variant), (1, 2));
    assert_eq!((gpu.runs, gpu.first_run, gpu.last_run), (2, 10, 40));
    assert!(cpu.is_current && !gpu.is_current);
}

#[test]
fn entry_uses_the_original_press_and_only_the_recording_start_snapshot() {
    let (_dir, stats) = manager();
    let pressed = Instant::now();
    stats.begin_run(config("original", "gpu"), pressed);
    let first = stats.take_run().unwrap();
    stats.begin_run(config("next", "cpu"), pressed + Duration::from_secs(5));
    let captured = first.captured(Some(pressed + Duration::from_millis(125)), true);
    assert_eq!(captured.entry_ms, Some(125.0));
    assert_eq!(captured.config["model"], "original");
    assert_eq!(stats.take_run().unwrap().config["model"], "next");
    assert!(stats.take_run().is_none());
}

#[test]
fn clear_removes_every_run_and_corrupt_rows_are_reported() {
    let (_dir, stats) = manager();
    stats
        .try_record_run(&run("m", RunResult::Text, 400.0))
        .unwrap();
    stats
        .connect()
        .unwrap()
        .execute("UPDATE transcription_runs SET config = 'invalid'", [])
        .unwrap();
    assert!(stats.summary(BTreeMap::new()).is_err());
    stats.clear().unwrap();
    assert!(stats.summary(BTreeMap::new()).unwrap().groups.is_empty());
}

#[test]
fn committed_runs_survive_forced_process_termination() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let (dir, stats) = manager();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "managers::stats::tests::persistence_child",
            "--nocapture",
        ])
        .env("HANDY_STATS_CRASH_TEST_PATH", dir.path().join("stats.db"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let committed = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .any(|line| line == "STATS_COMMITTED");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(committed, "writer must commit before being terminated");
    let summary = stats.summary(BTreeMap::new()).unwrap();
    assert_eq!(summary.groups[0].runs, 1);
    drop(stats);
    let reopened = StatsManager::at(dir.path().join("stats.db"));
    assert_eq!(reopened.summary(BTreeMap::new()).unwrap().groups[0].runs, 1);
}

#[test]
fn persistence_child() {
    use std::io::Write;
    let Some(path) = std::env::var_os("HANDY_STATS_CRASH_TEST_PATH") else {
        return;
    };
    let stats = StatsManager::at(path.into());
    stats
        .try_record_run(&run("m", RunResult::Text, 400.0))
        .unwrap();
    println!("STATS_COMMITTED");
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn language_is_resolved_or_selected_but_never_invented_for_auto_errors() {
    let decision = LanguageDecision {
        language: Some("fr".into()),
        source: LanguageSource::Selected,
    };
    assert_eq!(run_language(Some(&decision), "auto"), Some("fr".into()));
    assert_eq!(run_language(None, "de"), Some("de".into()));
    assert_eq!(run_language(None, "auto"), None);
}

#[test]
fn the_snapshot_names_the_model_and_no_secrets() {
    let snapshot = config_snapshot(&get_default_settings(), "whisper-small");
    assert_eq!(snapshot["model"], "whisper-small");
    assert_eq!(snapshot["selected_language"], "auto");
    assert_eq!(snapshot["accelerator"], "auto");
    assert!(snapshot.keys().all(|key| !key.contains("key")));
}

#[test]
fn post_processing_is_separate_and_skipped_calls_do_not_invent_samples() {
    let (dir, stats) = manager();
    for (result, elapsed) in [
        (RunResult::Text, Some(250.0)),
        (RunResult::Error, Some(500.0)),
        (RunResult::Text, None),
    ] {
        stats
            .try_record_run(&RunRecord {
                post_process_ms: elapsed,
                ..run("m", result, 1000.0)
            })
            .unwrap();
    }
    let summary = stats.summary(BTreeMap::new()).unwrap();
    let group = &summary.groups[0];
    assert_eq!(group.exit_ms.unwrap().min, 750.0);
    assert_eq!(group.exit_ms.unwrap().max, 1000.0);
    assert_eq!(group.post_process_ms.unwrap().min, 250.0);
    assert_eq!(group.post_process_ms.unwrap().max, 500.0);
    assert_eq!(
        (group.runs, group.failures, group.legacy_timing_runs),
        (3, 1, 0)
    );
    drop(stats);
    let reopened = StatsManager::at(dir.path().join("stats.db"));
    assert_eq!(
        reopened.summary(BTreeMap::new()).unwrap().groups[0].post_process_ms,
        group.post_process_ms
    );
}

#[test]
fn migration_preserves_inclusive_history_without_relabeling_it_as_exclusive() {
    let (dir, stats) = manager();
    let mut conn = Connection::open(dir.path().join("stats.db")).unwrap();
    Migrations::new(MIGRATIONS[..1].to_vec())
        .to_latest(&mut conn)
        .unwrap();
    conn.execute(
        "INSERT INTO transcription_runs
         (timestamp, language, entry_ms, exit_ms, result, speech_detected, config)
         VALUES (1, 'en', 50, 2300, 'text', 1, ?1)",
        [serde_json::to_string(&config("m", "gpu")).unwrap()],
    )
    .unwrap();
    drop(conn);
    let legacy = stats.summary(BTreeMap::new()).unwrap();
    let group = &legacy.groups[0];
    assert_eq!((group.runs, group.legacy_timing_runs), (1, 1));
    assert_eq!(group.entry_ms.unwrap().median, 50.0);
    assert!(group.exit_ms.is_none() && group.post_process_ms.is_none());
    let original: f64 = stats
        .connect()
        .unwrap()
        .query_row(
            "SELECT exit_ms FROM transcription_runs WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(original, 2300.0);
    stats
        .try_record_run(&RunRecord {
            post_process_ms: Some(200.0),
            ..run("m", RunResult::Text, 800.0)
        })
        .unwrap();
    let mixed = stats.summary(BTreeMap::new()).unwrap();
    assert_eq!(mixed.groups[0].runs, 2);
    assert_eq!(mixed.groups[0].exit_ms.unwrap().median, 600.0);
    assert_eq!(mixed.groups[0].legacy_timing_runs, 1);
}

#[test]
fn impossible_post_processing_durations_are_rejected_not_clamped() {
    let (_dir, stats) = manager();
    for elapsed in [-1.0, 1001.0, f64::NAN, f64::INFINITY] {
        assert!(stats
            .try_record_run(&RunRecord {
                post_process_ms: Some(elapsed),
                ..run("m", RunResult::Text, 1000.0)
            })
            .is_err());
    }
    assert!(stats.summary(BTreeMap::new()).unwrap().groups.is_empty());
}
