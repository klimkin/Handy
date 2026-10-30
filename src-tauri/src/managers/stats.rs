mod boxplot;
#[cfg(test)]
mod tests;

pub use boxplot::BoxStats;

use anyhow::{ensure, Result};
use log::warn;
use rusqlite::{params, Connection};
use rusqlite_migration::{Migrations, M};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::AppHandle;

use crate::language_detection::LanguageDecision;
use crate::settings::AppSettings;

static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE transcription_runs (
        id INTEGER PRIMARY KEY,
        timestamp INTEGER NOT NULL,
        language TEXT,
        entry_ms REAL,
        exit_ms REAL NOT NULL,
        result TEXT NOT NULL,
        speech_detected INTEGER NOT NULL,
        config TEXT NOT NULL
    );",
    ),
    M::up(
        "ALTER TABLE transcription_runs ADD COLUMN exit_exclusive_ms REAL;
     ALTER TABLE transcription_runs ADD COLUMN post_process_ms REAL;",
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunResult {
    Text,
    Empty,
    Error,
}

impl RunResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Empty => "empty",
            Self::Error => "error",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "text" => Some(Self::Text),
            "empty" => Some(Self::Empty),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

pub struct StartedRun {
    pressed_at: Instant,
    config: BTreeMap<String, String>,
}

impl StartedRun {
    pub fn captured(self, first_audio: Option<Instant>, speech_detected: bool) -> RunRecord {
        let language = run_language(None, &self.config["selected_language"]);
        RunRecord {
            entry_ms: first_audio
                .map(|at| at.saturating_duration_since(self.pressed_at).as_secs_f64() * 1000.0),
            config: self.config,
            language,
            release_to_paste_ms: 0.0,
            post_process_ms: None,
            result: RunResult::Empty,
            speech_detected,
        }
    }
}

#[derive(Clone)]
pub struct RunRecord {
    pub config: BTreeMap<String, String>,
    pub language: Option<String>,
    pub entry_ms: Option<f64>,
    pub release_to_paste_ms: f64,
    pub post_process_ms: Option<f64>,
    pub result: RunResult,
    pub speech_detected: bool,
}

/// A recording-start snapshot. Text, credentials and local paths are excluded.
pub fn config_snapshot(settings: &AppSettings, model: &str) -> BTreeMap<String, String> {
    [
        ("model", model.to_string()),
        ("requested_model", settings.selected_model.clone()),
        ("selected_language", settings.selected_language.clone()),
        ("spoken_languages", settings.spoken_languages.join(", ")),
        (
            "translate_to_english",
            settings.translate_to_english.to_string(),
        ),
        (
            "accelerator",
            setting_name(&settings.transcribe_accelerator),
        ),
        (
            "gpu_device",
            settings.transcribe_gpu_device.clone().unwrap_or_default(),
        ),
        ("vad_enabled", settings.vad_enabled.to_string()),
        ("vad_backend", setting_name(&settings.vad_backend)),
        (
            "long_form_chunking",
            settings.long_form_chunking.to_string(),
        ),
        ("lazy_stream_close", settings.lazy_stream_close.to_string()),
        (
            "always_on_microphone",
            settings.always_on_microphone.to_string(),
        ),
        (
            "shortcut_activation",
            setting_name(&settings.shortcut_activation),
        ),
        (
            "language_detection",
            setting_name(&settings.language_detection),
        ),
        (
            "model_unload_timeout",
            setting_name(&settings.model_unload_timeout),
        ),
        ("post_process", settings.post_process_enabled.to_string()),
        ("paste_method", setting_name(&settings.paste_method)),
        ("paste_delay_ms", settings.paste_delay_ms.to_string()),
        (
            "paste_delay_after_ms",
            settings.paste_delay_after_ms.to_string(),
        ),
        (
            "extra_recording_buffer_ms",
            settings.extra_recording_buffer_ms.to_string(),
        ),
        (
            "post_process_provider",
            settings.post_process_provider_id.clone(),
        ),
        (
            "post_process_model",
            settings
                .post_process_models
                .get(&settings.post_process_provider_id)
                .cloned()
                .unwrap_or_default(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

fn setting_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn run_language(resolved: Option<&LanguageDecision>, selected: &str) -> Option<String> {
    resolved
        .and_then(|decision| decision.language.clone())
        .or_else(|| (selected != "auto").then(|| selected.to_string()))
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct StatsGroup {
    pub model: String,
    pub language: Option<String>,
    pub variant: u32,
    pub config: BTreeMap<String, String>,
    pub is_current: bool,
    pub runs: u32,
    pub failures: u32,
    pub first_run: i64,
    pub last_run: i64,
    /// Key press to first captured audio, including runs where later processing failed.
    pub entry_ms: Option<BoxStats>,
    /// Stop key edge to completed output minus post-processing, successful runs only.
    pub exit_ms: Option<BoxStats>,
    /// Actual post-processing attempts, including failed attempts.
    pub post_process_ms: Option<BoxStats>,
    /// Older runs whose inclusive Exit cannot be separated retrospectively.
    pub legacy_timing_runs: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct StatsSummary {
    pub current: BTreeMap<String, String>,
    pub groups: Vec<StatsGroup>,
}

struct StoredRun {
    timestamp: i64,
    record: RunRecord,
    exit_ms: Option<f64>,
}

#[derive(Default)]
struct Accumulator {
    first_order: usize,
    runs: u32,
    failures: u32,
    first_run: i64,
    last_run: i64,
    entry_ms: Vec<f64>,
    exit_ms: Vec<f64>,
    post_process_ms: Vec<f64>,
    legacy_timing_runs: u32,
}

fn summarize(runs: Vec<StoredRun>, current: BTreeMap<String, String>) -> StatsSummary {
    let mut groups: BTreeMap<(Option<String>, BTreeMap<String, String>), Accumulator> =
        BTreeMap::new();
    for (
        first_order,
        StoredRun {
            timestamp,
            record,
            exit_ms,
        },
    ) in runs.into_iter().enumerate()
    {
        if record.result == RunResult::Empty && !record.speech_detected {
            continue;
        }
        let key = (record.language, record.config);
        let acc = groups.entry(key).or_insert_with(|| Accumulator {
            first_order,
            first_run: timestamp,
            last_run: timestamp,
            ..Default::default()
        });
        acc.runs += 1;
        acc.first_run = acc.first_run.min(timestamp);
        acc.last_run = acc.last_run.max(timestamp);
        if let Some(entry) = record.entry_ms {
            acc.entry_ms.push(entry);
        }
        if let Some(elapsed) = record.post_process_ms {
            acc.post_process_ms.push(elapsed);
        }
        if exit_ms.is_none() {
            acc.legacy_timing_runs += 1;
        }
        if record.result == RunResult::Text {
            if let Some(exit) = exit_ms {
                acc.exit_ms.push(exit);
            }
        } else {
            acc.failures += 1;
        }
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_key(|(_, acc)| acc.first_order);
    let mut groups: Vec<StatsGroup> = groups
        .into_iter()
        .map(|((language, config), acc)| StatsGroup {
            model: config.get("model").cloned().unwrap_or_default(),
            language,
            variant: 0,
            is_current: config == current,
            config,
            runs: acc.runs,
            failures: acc.failures,
            first_run: acc.first_run,
            last_run: acc.last_run,
            entry_ms: BoxStats::of(acc.entry_ms),
            exit_ms: BoxStats::of(acc.exit_ms),
            post_process_ms: BoxStats::of(acc.post_process_ms),
            legacy_timing_runs: acc.legacy_timing_runs,
        })
        .collect();
    let mut variants = BTreeMap::new();
    for group in &mut groups {
        let variant = variants
            .entry((group.model.clone(), group.language.clone()))
            .or_insert(0);
        *variant += 1;
        group.variant = *variant;
    }
    StatsSummary { current, groups }
}

pub struct StatsManager {
    db_path: PathBuf,
    started: Mutex<Option<StartedRun>>,
}

impl StatsManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        let manager = Self::at(crate::portable::app_data_dir(app_handle)?.join("stats.db"));
        if let Err(error) = manager.connect() {
            warn!("Statistics database unavailable: {error}");
        }
        Ok(manager)
    }

    fn at(db_path: PathBuf) -> Self {
        Self {
            db_path,
            started: Mutex::new(None),
        }
    }

    pub fn begin_run(&self, config: BTreeMap<String, String>, pressed_at: Instant) {
        *self.started.lock().unwrap() = Some(StartedRun { pressed_at, config });
    }

    pub fn take_run(&self) -> Option<StartedRun> {
        self.started.lock().unwrap().take()
    }

    fn connect(&self) -> Result<Connection> {
        let mut conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
        Migrations::new(MIGRATIONS.to_vec()).to_latest(&mut conn)?;
        Ok(conn)
    }

    pub fn record_run(&self, run: &RunRecord) {
        if let Err(error) = self.try_record_run(run) {
            warn!("Failed to record transcription statistics: {error}");
        }
    }

    fn try_record_run(&self, run: &RunRecord) -> Result<()> {
        let post_process = run.post_process_ms.unwrap_or(0.0);
        ensure!(
            run.release_to_paste_ms.is_finite()
                && post_process.is_finite()
                && post_process >= 0.0
                && run.release_to_paste_ms >= post_process,
            "Invalid statistics timing: post-processing must fit inside release-to-paste"
        );
        self.connect()?.execute(
            "INSERT INTO transcription_runs
                (timestamp, language, entry_ms, exit_ms, result, speech_detected, config,
                 exit_exclusive_ms, post_process_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                chrono::Utc::now().timestamp(),
                run.language,
                run.entry_ms,
                run.release_to_paste_ms,
                run.result.as_str(),
                run.speech_detected,
                serde_json::to_string(&run.config)?,
                run.release_to_paste_ms - post_process,
                run.post_process_ms,
            ],
        )?;
        Ok(())
    }

    pub fn summary(&self, current: BTreeMap<String, String>) -> Result<StatsSummary> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT timestamp, language, entry_ms, exit_ms, result, speech_detected, config,
                    exit_exclusive_ms, post_process_ms
             FROM transcription_runs ORDER BY id",
        )?;
        let mut rows = statement.query([])?;
        let mut runs = Vec::new();
        while let Some(row) = rows.next()? {
            let result: String = row.get(4)?;
            let Some(result) = RunResult::parse(&result) else {
                continue;
            };
            let config: String = row.get(6)?;
            runs.push(StoredRun {
                timestamp: row.get(0)?,
                exit_ms: row.get(7)?,
                record: RunRecord {
                    language: row.get(1)?,
                    entry_ms: row.get(2)?,
                    release_to_paste_ms: row.get(3)?,
                    post_process_ms: row.get(8)?,
                    result,
                    speech_detected: row.get(5)?,
                    config: serde_json::from_str(&config)?,
                },
            });
        }
        Ok(summarize(runs, current))
    }

    pub fn clear(&self) -> Result<()> {
        self.connect()?
            .execute("DELETE FROM transcription_runs", [])?;
        Ok(())
    }
}
