use crate::managers::model::{hf_complete_snapshot, HfFile};
use crate::settings::{
    AppSettings, OpenVinoDevice, OPENVINO_DEFAULT_MODEL_ID, OPENVINO_PROVIDER_ID,
};
use log::{error, info, warn};
use openvino_genai::{
    ChatHistory, ChatMessage, GenerationConfig, LlmPipeline, Streamer, StreamingStatus,
};
use serde::Serialize;
use specta::Type;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// NPU static shapes, in tokens. Unmeasured proposals, not crate requirements.
const MAX_PROMPT_LEN: usize = 1024;
const MIN_RESPONSE_LEN: usize = 512;
/// Unmeasured stand-in for characters per token, used to keep a prompt inside
/// the NPU's static prompt shape without running the tokenizer.
const CHARS_PER_TOKEN: usize = 4;

fn device_name(device: OpenVinoDevice) -> &'static str {
    match device {
        OpenVinoDevice::Npu => "NPU",
        OpenVinoDevice::Gpu => "GPU",
        OpenVinoDevice::Cpu => "CPU",
    }
}

pub struct OfferedModel {
    pub id: &'static str,
    pub label: &'static str,
    pub repo: &'static str,
    pub revision: &'static str,
    pub files: &'static [HfFile],
}

const fn file(name: &'static str, size_bytes: u64) -> HfFile {
    HfFile { name, size_bytes }
}

pub const OFFERED_MODELS: &[OfferedModel] = &[OfferedModel {
    id: OPENVINO_DEFAULT_MODEL_ID,
    label: "Qwen2.5 1.5B Instruct",
    repo: "OpenVINO/Qwen2.5-1.5B-Instruct-int4-ov",
    revision: "4d14c299e35d8b74e3471f9e92bd1377fae50736",
    files: &[
        file("openvino_model.xml", 2_435_810),
        file("openvino_model.bin", 909_864_660),
        file("openvino_tokenizer.xml", 25_114),
        file("openvino_tokenizer.bin", 5_588_628),
        file("openvino_detokenizer.xml", 9_594),
        file("openvino_detokenizer.bin", 2_189_639),
        file("tokenizer.json", 11_421_896),
        file("tokenizer_config.json", 7_336),
        file("config.json", 772),
        file("generation_config.json", 242),
        file("special_tokens_map.json", 613),
    ],
}];

pub fn offered_model(id: &str) -> Option<&'static OfferedModel> {
    OFFERED_MODELS.iter().find(|model| model.id == id)
}

/// A snapshot directory that holds every file of one offered model.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelDir(PathBuf);

impl ModelDir {
    fn find(model: &OfferedModel) -> Option<Self> {
        hf_complete_snapshot(model.repo, model.revision, model.files).map(ModelDir)
    }
}

type LoadKey = (ModelDir, OpenVinoDevice);

enum Slot {
    Empty,
    Loaded { key: LoadKey, pipeline: LlmPipeline },
}

/// Owns at most one compiled pipeline. Tauri-managed.
pub struct OpenVinoCleaner {
    cache_dir: PathBuf,
    slot: Arc<Mutex<Slot>>,
}

impl OpenVinoCleaner {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            slot: Arc::new(Mutex::new(Slot::Empty)),
        }
    }

    /// Loads the selected model on the selected device without generating.
    pub async fn prepare(&self, settings: &AppSettings) -> Result<(), LoadError> {
        let key = resolve_key(settings)?;
        self.with_pipeline(key, |_| Ok(()), LoadError::DeviceFailed)
            .await
    }

    pub async fn clean(
        &self,
        settings: &AppSettings,
        system_prompt: &str,
        transcription: &str,
    ) -> Result<String, CleanerError> {
        check_prompt_budget(
            settings.post_process_openvino_device,
            system_prompt,
            transcription,
        )?;
        let key = resolve_key(settings)?;
        let messages = [
            ChatMessage::system(system_prompt),
            ChatMessage::user(transcription),
        ];
        let stop = StopOnDrop::default();
        let stopped = Arc::clone(&stop.0);
        self.with_pipeline(
            key,
            move |pipeline| generate(pipeline, &messages, stopped),
            CleanerError::GenerateFailed,
        )
        .await
    }

    pub fn unload(&self) {
        let slot = Arc::clone(&self.slot);
        tauri::async_runtime::spawn_blocking(move || {
            *lock_slot(&slot) = Slot::Empty;
        });
    }

    async fn with_pipeline<T, E, F>(&self, key: LoadKey, work: F, panicked: E) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<LoadError> + Send + 'static,
        F: FnOnce(&mut LlmPipeline) -> Result<T, E> + Send + 'static,
    {
        let slot = Arc::clone(&self.slot);
        let cache_dir = self.cache_dir.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let mut slot = lock_slot(&slot);
            work(load_or_reuse(&mut slot, key, &cache_dir)?)
        })
        .await
        .unwrap_or_else(|e| {
            error!("OpenVINO worker panicked: {e}");
            Err(panicked)
        })
    }
}

/// Asks generation to stop once nobody awaits it, so a cancelled dictation
/// releases the device at the next token instead of after the whole reply.
#[derive(Default)]
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn streaming_status(stopped: &AtomicBool) -> StreamingStatus {
    if stopped.load(Ordering::Relaxed) {
        StreamingStatus::Stop
    } else {
        StreamingStatus::Running
    }
}

fn lock_slot(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

fn resolve_key(settings: &AppSettings) -> Result<LoadKey, LoadError> {
    let model_id = settings
        .post_process_models
        .get(OPENVINO_PROVIDER_ID)
        .map(String::as_str)
        .unwrap_or_default();
    let model = offered_model(model_id).ok_or(LoadError::UnknownModel)?;
    let dir = ModelDir::find(model).ok_or(LoadError::ModelNotDownloaded)?;
    Ok((dir, settings.post_process_openvino_device))
}

fn load_or_reuse<'a>(
    slot: &'a mut Slot,
    key: LoadKey,
    cache_dir: &Path,
) -> Result<&'a mut LlmPipeline, LoadError> {
    let reusable = matches!(&*slot, Slot::Loaded { key: loaded, .. } if *loaded == key);
    if !reusable {
        // Dropped before compiling so two models never sit on the device.
        *slot = Slot::Empty;
        let pipeline = load_pipeline(&key, cache_dir)?;
        *slot = Slot::Loaded { key, pipeline };
    }
    match slot {
        Slot::Loaded { pipeline, .. } => Ok(pipeline),
        Slot::Empty => Err(LoadError::DeviceFailed),
    }
}

fn load_pipeline((dir, device): &LoadKey, cache_dir: &Path) -> Result<LlmPipeline, LoadError> {
    openvino_genai::load().map_err(|e| {
        warn!("OpenVINO GenAI library not found: {e}");
        LoadError::LibraryMissing
    })?;
    let properties = load_properties(*device, &cache_dir.to_string_lossy());
    let properties: Vec<(&str, &str)> = properties
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let device = device_name(*device);
    info!("Loading {} on {device}", dir.0.display());
    LlmPipeline::with_properties(&dir.0.to_string_lossy(), device, &properties).map_err(|e| {
        error!("OpenVINO could not load the model on {device}: {e}");
        LoadError::DeviceFailed
    })
}

fn generate(
    pipeline: &mut LlmPipeline,
    messages: &[ChatMessage],
    stopped: Arc<AtomicBool>,
) -> Result<String, CleanerError> {
    let mut history = ChatHistory::new().map_err(generate_failed)?;
    for message in messages {
        history.push(message).map_err(generate_failed)?;
    }
    let mut config = GenerationConfig::new().map_err(generate_failed)?;
    config
        .set_max_new_tokens(MIN_RESPONSE_LEN)
        .map_err(generate_failed)?;
    let streamer = Streamer::new(move |_| streaming_status(&stopped));
    let (text, generated_tokens) = pipeline
        .generate_with_history(&history, Some(&config), Some(&streamer))
        .and_then(|results| {
            let generated_tokens = results.get_perf_metrics()?.get_num_generation_tokens()?;
            Ok((results.get_string()?, generated_tokens))
        })
        .map_err(generate_failed)?;
    check_reply(text, generated_tokens)
}

fn check_reply(text: String, generated_tokens: usize) -> Result<String, CleanerError> {
    if generated_tokens >= MIN_RESPONSE_LEN {
        return Err(CleanerError::ReplyCutOff);
    }
    if text.trim().is_empty() {
        return Err(CleanerError::EmptyOutput);
    }
    Ok(text)
}

fn generate_failed(e: impl fmt::Display) -> CleanerError {
    error!("OpenVINO generation failed: {e}");
    CleanerError::GenerateFailed
}

/// Why the model could not be loaded. The settings screen shows each one.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum LoadError {
    UnknownModel,
    ModelNotDownloaded,
    LibraryMissing,
    DeviceFailed,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LoadError::UnknownModel => "the selected model is not offered",
            LoadError::ModelNotDownloaded => "the model is not downloaded",
            LoadError::LibraryMissing => "the OpenVINO GenAI library was not found",
            LoadError::DeviceFailed => "the model could not be loaded on the device",
        })
    }
}

impl std::error::Error for LoadError {}

/// Why a cleanup produced no text. Dictation reports these as the
/// post-processing notice, so they stay in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanerError {
    Load(LoadError),
    GenerateFailed,
    EmptyOutput,
    /// The reply used every token it was allowed, so its end is missing.
    ReplyCutOff,
    PromptTooLong,
}

impl From<LoadError> for CleanerError {
    fn from(error: LoadError) -> Self {
        CleanerError::Load(error)
    }
}

impl fmt::Display for CleanerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CleanerError::Load(error) => error.fmt(f),
            CleanerError::GenerateFailed => f.write_str("generation failed"),
            CleanerError::EmptyOutput => f.write_str("the model returned no text"),
            CleanerError::ReplyCutOff => {
                write!(f, "the reply reached the {MIN_RESPONSE_LEN}-token limit")
            }
            CleanerError::PromptTooLong => f.write_str("the prompt is too long for the NPU"),
        }
    }
}

impl std::error::Error for CleanerError {}

fn check_prompt_budget(
    device: OpenVinoDevice,
    system_prompt: &str,
    transcription: &str,
) -> Result<(), CleanerError> {
    let chars = system_prompt.chars().count() + transcription.chars().count();
    if device == OpenVinoDevice::Npu && chars > MAX_PROMPT_LEN * CHARS_PER_TOKEN {
        return Err(CleanerError::PromptTooLong);
    }
    Ok(())
}

fn load_properties(device: OpenVinoDevice, cache_dir: &str) -> Vec<(&'static str, String)> {
    let mut properties = vec![("CACHE_DIR", cache_dir.to_string())];
    if device == OpenVinoDevice::Npu {
        properties.push(("MAX_PROMPT_LEN", MAX_PROMPT_LEN.to_string()));
        properties.push(("MIN_RESPONSE_LEN", MIN_RESPONSE_LEN.to_string()));
    }
    properties
}

#[cfg(test)]
mod tests {
    use super::*;

    fn property<'a>(properties: &'a [(&str, String)], key: &str) -> Option<&'a str> {
        properties
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn generation_stops_once_the_caller_stops_waiting() {
        let stop = StopOnDrop::default();
        let stopped = Arc::clone(&stop.0);
        assert!(matches!(
            streaming_status(&stopped),
            StreamingStatus::Running
        ));
        drop(stop);
        assert!(matches!(streaming_status(&stopped), StreamingStatus::Stop));
    }

    #[test]
    fn npu_properties_fix_the_static_shapes() {
        let properties = load_properties(OpenVinoDevice::Npu, "cache");
        assert_eq!(property(&properties, "CACHE_DIR"), Some("cache"));
        assert_eq!(property(&properties, "MAX_PROMPT_LEN"), Some("1024"));
        assert_eq!(property(&properties, "MIN_RESPONSE_LEN"), Some("512"));
    }

    #[test]
    fn cpu_properties_only_cache() {
        let properties = load_properties(OpenVinoDevice::Cpu, "cache");
        assert_eq!(property(&properties, "CACHE_DIR"), Some("cache"));
        assert_eq!(property(&properties, "MAX_PROMPT_LEN"), None);
    }

    #[test]
    fn a_reply_that_used_every_token_is_cut_off() {
        assert_eq!(
            check_reply("cleaned".into(), MIN_RESPONSE_LEN),
            Err(CleanerError::ReplyCutOff)
        );
        assert_eq!(
            check_reply("cleaned".into(), MIN_RESPONSE_LEN - 1),
            Ok("cleaned".into())
        );
        assert_eq!(check_reply("  ".into(), 1), Err(CleanerError::EmptyOutput));
    }

    #[test]
    fn a_prompt_over_the_npu_budget_is_too_long() {
        let system = "a".repeat(4000);
        let long = "b".repeat(97);
        assert_eq!(
            check_prompt_budget(OpenVinoDevice::Npu, &system, &long),
            Err(CleanerError::PromptTooLong)
        );
        assert_eq!(
            check_prompt_budget(OpenVinoDevice::Npu, &system, &"b".repeat(96)),
            Ok(())
        );
        assert_eq!(
            check_prompt_budget(OpenVinoDevice::Cpu, &system, &long),
            Ok(())
        );
        assert_eq!(
            check_prompt_budget(OpenVinoDevice::Gpu, &system, &long),
            Ok(())
        );
    }
}
