mod boxplot;
mod export;
#[cfg(test)]
mod tests;
pub mod trace;

pub use boxplot::BoxStats;

use anyhow::Result;
use log::warn;
use rusqlite::{params, Connection};
use rusqlite_migration::{Migrations, M};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Condvar, Mutex,
};
use std::time::Duration;
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
    M::up("CREATE TABLE telemetry_state (id INTEGER PRIMARY KEY CHECK(id=1), generation INTEGER NOT NULL, clear_pending INTEGER NOT NULL, next_segment INTEGER NOT NULL); INSERT INTO telemetry_state VALUES(1,0,0,1); CREATE TABLE dictation_traces (run_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, timestamp INTEGER NOT NULL, config TEXT NOT NULL, language TEXT, outcome TEXT NOT NULL, capture_gap_us INTEGER, capture_reason TEXT, output_gap_us INTEGER, output_reason TEXT, payload TEXT, exported INTEGER NOT NULL DEFAULT 0);"),
];

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
        ("post_process", "false".into()),
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

/// Snapshot requested postprocessing independently of persisted enable state.
pub fn requested_config(
    settings: &AppSettings,
    model: &str,
    post_process: bool,
) -> BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    let mut config = config_snapshot(settings, model);
    config.insert("post_process".into(), post_process.to_string());
    config.extend([
        ("auto_submit".into(), settings.auto_submit.to_string()),
        (
            "auto_submit_key".into(),
            setting_name(&settings.auto_submit_key),
        ),
        (
            "clipboard_handling".into(),
            setting_name(&settings.clipboard_handling),
        ),
        ("reliable_paste".into(), settings.reliable_paste.to_string()),
        (
            "append_trailing_space".into(),
            settings.append_trailing_space.to_string(),
        ),
        ("typing_tool".into(), setting_name(&settings.typing_tool)),
    ]);
    if post_process {
        config.insert(
            "post_process_provider".into(),
            settings.post_process_provider_id.clone(),
        );
        config.insert(
            "post_process_model".into(),
            settings
                .post_process_models
                .get(&settings.post_process_provider_id)
                .cloned()
                .unwrap_or_default(),
        );
        if let Some(prompt) = settings
            .post_process_selected_prompt_id
            .as_ref()
            .and_then(|id| settings.post_process_prompts.iter().find(|p| &p.id == id))
        {
            config.insert("post_process_prompt".into(), prompt.id.clone());
            config.insert(
                "post_process_prompt_sha256".into(),
                format!("{:x}", Sha256::digest(prompt.prompt.as_bytes())),
            );
        }
    }
    config
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
    pub cancellations: u32,
    pub first_run: i64,
    pub last_run: i64,
    pub capture_gap_ms: Option<BoxStats>,
    pub output_gap_ms: Option<BoxStats>,
    pub capture_missing: u32,
    pub output_missing: u32,
    pub capture_unavailable_reasons: BTreeMap<String, u32>,
    pub output_unavailable_reasons: BTreeMap<String, u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct StatsSummary {
    pub current: BTreeMap<String, String>,
    pub groups: Vec<StatsGroup>,
    pub legacy_runs: u32,
    pub trace_path: String,
    pub trace_error: Option<String>,
    pub pending_exports: u32,
}
#[derive(Default)]
struct Accumulator {
    order: usize,
    runs: u32,
    failures: u32,
    cancellations: u32,
    first: i64,
    last: i64,
    capture: Vec<f64>,
    output: Vec<f64>,
    capture_missing: u32,
    output_missing: u32,
    capture_reasons: BTreeMap<String, u32>,
    output_reasons: BTreeMap<String, u32>,
}

pub struct StatsManager {
    db_path: PathBuf,
    generation: Arc<AtomicU64>,
    export_tx: mpsc::SyncSender<export::Command>,
    export_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    export_error: Arc<Mutex<Option<String>>>,
    commit_error: Mutex<Option<String>>,
    owners: Mutex<usize>,
    shutting_down: AtomicBool,
    owners_changed: Condvar,
}
impl StatsManager {
    pub fn new(app: &AppHandle) -> Result<Self> {
        Ok(Self::at(
            crate::portable::app_data_dir(app)?.join("stats.db"),
        ))
    }
    #[cfg(test)]
    pub(crate) fn test_at(db_path: PathBuf) -> Self {
        Self::at(db_path)
    }
    fn at(db_path: PathBuf) -> Self {
        let generation = Self::connect_path(&db_path)
            .and_then(|c| {
                Ok(c.query_row(
                    "SELECT generation FROM telemetry_state WHERE id=1",
                    [],
                    |r| r.get::<_, u64>(0),
                )?)
            })
            .unwrap_or(0);
        let (tx, rx) = mpsc::sync_channel(4);
        let error = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(generation));
        let path = db_path.clone();
        let errors = Arc::clone(&error);
        let writer_generation = Arc::clone(&generation);
        let worker = std::thread::Builder::new()
            .name("dictation-trace-writer".into())
            .spawn(move || export::run(path, rx, errors, writer_generation))
            .ok();
        Self {
            db_path,
            generation,
            export_tx: tx,
            export_thread: Mutex::new(worker),
            export_error: error,
            commit_error: Mutex::new(None),
            owners: Mutex::new(0),
            shutting_down: AtomicBool::new(false),
            owners_changed: Condvar::new(),
        }
    }
    pub fn input_stamp(&self, origin: trace::InputOrigin) -> trace::InputStamp {
        let generation = self.generation.load(Ordering::Acquire);
        let mut stamp = trace::InputStamp::now(origin);
        stamp.generation = Some(generation);
        stamp
    }
    pub fn begin(
        self: &Arc<Self>,
        activation: trace::InputStamp,
        config: BTreeMap<String, String>,
    ) -> trace::RunOwner {
        let generation = activation
            .generation
            .unwrap_or_else(|| self.generation.load(Ordering::Acquire));
        trace::RunOwner::new(Arc::clone(self), activation, config, generation)
    }
    fn connect_path(path: &std::path::Path) -> Result<Connection> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        Migrations::new(MIGRATIONS.to_vec()).to_latest(&mut conn)?;
        Ok(conn)
    }
    fn connect(&self) -> Result<Connection> {
        Self::connect_path(&self.db_path)
    }
    pub fn trace_path(&self) -> PathBuf {
        self.db_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("telemetry/dictation-events.jsonl")
    }
    fn record_trace(&self, trace: &trace::FinalizedTrace, generation: u64) {
        let persist = || -> Result<()> {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let current: u64 = tx.query_row(
                "SELECT generation FROM telemetry_state WHERE id=1",
                [],
                |r| r.get(0),
            )?;
            if current != generation {
                return Ok(());
            }
            let timestamp =
                chrono::DateTime::parse_from_rfc3339(&trace.started_at_utc)?.timestamp();
            tx.execute("INSERT OR IGNORE INTO dictation_traces(run_id,generation,timestamp,config,language,outcome,capture_gap_us,capture_reason,output_gap_us,output_reason,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![trace.run_id,generation,timestamp,serde_json::to_string(&trace.config)?,trace.language,serde_json::to_value(trace.outcome)?.as_str().unwrap_or("processing_failed"),trace.metrics.capture_gap.us,trace.metrics.capture_gap.reason,trace.metrics.output_gap.us,trace.metrics.output_gap.reason,serde_json::to_string(trace)?])?;
            tx.commit()?;
            Ok(())
        };
        let mut result = persist();
        for _ in 0..2 {
            if result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            result = persist();
        }
        if let Err(error) = result {
            warn!("Failed to commit dictation trace: {error}");
            *self.commit_error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(format!("statistics_commit_failed: {error}"));
        } else {
            let _ = self.export_tx.try_send(export::Command::Wake);
        }
    }
    pub fn summary(&self, current: BTreeMap<String, String>) -> Result<StatsSummary> {
        let conn = self.connect()?;
        let mut stmt=conn.prepare("SELECT timestamp,config,language,outcome,capture_gap_us,capture_reason,output_gap_us,output_reason FROM dictation_traces ORDER BY rowid")?;
        let mut rows = stmt.query([])?;
        let mut groups: BTreeMap<(Option<String>, BTreeMap<String, String>), Accumulator> =
            BTreeMap::new();
        let mut order = 0;
        while let Some(row) = rows.next()? {
            let timestamp: i64 = row.get(0)?;
            let config: String = row.get(1)?;
            let config = serde_json::from_str(&config)?;
            let language = row.get(2)?;
            let outcome: String = row.get(3)?;
            let acc = groups
                .entry((language, config))
                .or_insert_with(|| Accumulator {
                    order,
                    first: timestamp,
                    last: timestamp,
                    ..Default::default()
                });
            order += 1;
            acc.runs += 1;
            acc.first = acc.first.min(timestamp);
            acc.last = acc.last.max(timestamp);
            if outcome == "cancelled" {
                acc.cancellations += 1;
                continue;
            }
            if outcome != "output_completed" && outcome != "no_output_requested" {
                acc.failures += 1;
            }
            if let Some(us) = row.get::<_, Option<i64>>(4)? {
                acc.capture.push(us as f64 / 1000.0);
            } else {
                acc.capture_missing += 1;
                *acc.capture_reasons
                    .entry(
                        row.get::<_, Option<String>>(5)?
                            .unwrap_or_else(|| "unavailable".into()),
                    )
                    .or_default() += 1;
            }
            if let Some(us) = row.get::<_, Option<i64>>(6)? {
                acc.output.push(us as f64 / 1000.0);
            } else {
                acc.output_missing += 1;
                *acc.output_reasons
                    .entry(
                        row.get::<_, Option<String>>(7)?
                            .unwrap_or_else(|| "unavailable".into()),
                    )
                    .or_default() += 1;
            }
        }
        let mut sorted = groups.into_iter().collect::<Vec<_>>();
        sorted.sort_by_key(|(_, a)| a.order);
        let mut variants = BTreeMap::new();
        let groups = sorted
            .into_iter()
            .map(|((language, config), a)| {
                let model = config.get("model").cloned().unwrap_or_default();
                let variant = variants
                    .entry((model.clone(), language.clone()))
                    .or_insert(0);
                *variant += 1;
                StatsGroup {
                    model,
                    language,
                    variant: *variant,
                    is_current: config == current,
                    config,
                    runs: a.runs,
                    failures: a.failures,
                    cancellations: a.cancellations,
                    first_run: a.first,
                    last_run: a.last,
                    capture_gap_ms: BoxStats::of(a.capture),
                    output_gap_ms: BoxStats::of(a.output),
                    capture_missing: a.capture_missing,
                    output_missing: a.output_missing,
                    capture_unavailable_reasons: a.capture_reasons,
                    output_unavailable_reasons: a.output_reasons,
                }
            })
            .collect();
        Ok(StatsSummary {
            current,
            groups,
            legacy_runs: conn
                .query_row("SELECT COUNT(*) FROM transcription_runs", [], |r| r.get(0))?,
            trace_path: self.trace_path().to_string_lossy().into_owned(),
            trace_error: self
                .commit_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .or_else(|| {
                    self.export_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                }),
            pending_exports: conn.query_row(
                "SELECT COUNT(*) FROM dictation_traces WHERE exported=0",
                [],
                |r| r.get(0),
            )?,
        })
    }
    pub fn clear(&self) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.export_tx.send(export::Command::Clear(tx))?;
        rx.recv()?.map_err(anyhow::Error::msg)?;
        Ok(())
    }
    pub fn stop_admissions(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }
    fn owner_started(&self) {
        *self.owners.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }
    fn owner_finished(&self) {
        let mut owners = self.owners.lock().unwrap_or_else(|e| e.into_inner());
        *owners = owners.saturating_sub(1);
        self.owners_changed.notify_all();
    }
    pub fn shutdown(&self) -> Result<()> {
        self.stop_admissions();
        let owners = self.owners.lock().unwrap_or_else(|e| e.into_inner());
        let (owners, _) = self
            .owners_changed
            .wait_timeout_while(owners, Duration::from_secs(12), |owners| *owners > 0)
            .unwrap_or_else(|e| e.into_inner());
        let active = *owners;
        drop(owners);
        let (tx, rx) = mpsc::sync_channel(1);
        self.export_tx.send(export::Command::Shutdown(tx))?;
        rx.recv()?.map_err(anyhow::Error::msg)?;
        if let Some(handle) = self
            .export_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        anyhow::ensure!(active==0,"{active} active dictation traces did not settle before shutdown; committed traces drained");
        Ok(())
    }
}
impl Drop for StatsManager {
    fn drop(&mut self) {
        let _ = self.export_tx.try_send(export::Command::Stop);
    }
}
