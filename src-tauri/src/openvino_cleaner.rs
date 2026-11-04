use crate::managers::model::{hf_snapshot_dirs, ModelManager};
use crate::settings::{
    self, AppSettings, OpenVinoDevice, OPENVINO_DEFAULT_MODEL_ID, OPENVINO_PROVIDER_ID,
};
use log::{error, info, warn};
use openvino_genai::{GenerationConfig, LlmPipeline};
use serde::Serialize;
use specta::Type;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tauri::{AppHandle, Emitter, State};

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
    pub files: &'static [&'static str],
}

pub const OFFERED_MODELS: &[OfferedModel] = &[OfferedModel {
    id: OPENVINO_DEFAULT_MODEL_ID,
    label: "Qwen2.5 1.5B Instruct",
    repo: "OpenVINO/Qwen2.5-1.5B-Instruct-int4-ov",
    revision: "4d14c299e35d8b74e3471f9e92bd1377fae50736",
    files: &[
        "openvino_model.xml",
        "openvino_model.bin",
        "openvino_tokenizer.xml",
        "openvino_tokenizer.bin",
        "openvino_detokenizer.xml",
        "openvino_detokenizer.bin",
        "tokenizer.json",
        "tokenizer_config.json",
        "config.json",
        "generation_config.json",
        "special_tokens_map.json",
    ],
}];

fn offered_model(id: &str) -> Option<&'static OfferedModel> {
    OFFERED_MODELS.iter().find(|model| model.id == id)
}

/// A snapshot directory that holds every file of one offered model.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelDir(PathBuf);

impl ModelDir {
    fn find(model: &OfferedModel) -> Option<Self> {
        hf_snapshot_dirs(model.repo, model.revision)
            .into_iter()
            .find(|dir| model.files.iter().all(|file| dir.join(file).is_file()))
            .map(ModelDir)
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
    pub async fn prepare(&self, settings: &AppSettings) -> Result<(), CleanerError> {
        let key = resolve_key(settings)?;
        self.with_pipeline(key, |_| Ok(())).await
    }

    pub async fn clean(
        &self,
        settings: &AppSettings,
        template: &str,
        transcription: &str,
    ) -> Result<String, CleanerError> {
        let prompt = fill_prompt(template, transcription);
        check_prompt_budget(settings.post_process_openvino_device, &prompt)?;
        let key = resolve_key(settings)?;
        self.with_pipeline(key, move |pipeline| generate(pipeline, &prompt))
            .await
    }

    pub fn unload(&self) {
        let slot = Arc::clone(&self.slot);
        tauri::async_runtime::spawn_blocking(move || {
            *lock_slot(&slot) = Slot::Empty;
        });
    }

    async fn with_pipeline<T, F>(&self, key: LoadKey, work: F) -> Result<T, CleanerError>
    where
        T: Send + 'static,
        F: FnOnce(&mut LlmPipeline) -> Result<T, CleanerError> + Send + 'static,
    {
        let slot = Arc::clone(&self.slot);
        let cache_dir = self.cache_dir.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let mut slot = lock_slot(&slot);
            let result = load_or_reuse(&mut slot, key, &cache_dir).and_then(work);
            if matches!(result, Err(CleanerError::GenerateFailed)) {
                *slot = Slot::Empty;
            }
            result
        })
        .await
        .unwrap_or_else(|e| {
            error!("OpenVINO worker panicked: {e}");
            Err(CleanerError::GenerateFailed)
        })
    }
}

fn lock_slot(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(|poisoned| {
        slot.clear_poison();
        let mut guard = poisoned.into_inner();
        *guard = Slot::Empty;
        guard
    })
}

fn resolve_key(settings: &AppSettings) -> Result<LoadKey, CleanerError> {
    let model_id = settings
        .post_process_models
        .get(OPENVINO_PROVIDER_ID)
        .map(String::as_str)
        .unwrap_or_default();
    let model = offered_model(model_id).ok_or(CleanerError::UnknownModel)?;
    let dir = ModelDir::find(model).ok_or(CleanerError::ModelNotDownloaded)?;
    Ok((dir, settings.post_process_openvino_device))
}

fn load_or_reuse<'a>(
    slot: &'a mut Slot,
    key: LoadKey,
    cache_dir: &Path,
) -> Result<&'a mut LlmPipeline, CleanerError> {
    let reusable = matches!(&*slot, Slot::Loaded { key: loaded, .. } if *loaded == key);
    if !reusable {
        // Dropped before compiling so two models never sit on the device.
        *slot = Slot::Empty;
        let pipeline = load_pipeline(&key, cache_dir)?;
        *slot = Slot::Loaded { key, pipeline };
    }
    match slot {
        Slot::Loaded { pipeline, .. } => Ok(pipeline),
        Slot::Empty => Err(CleanerError::DeviceFailed),
    }
}

fn load_pipeline((dir, device): &LoadKey, cache_dir: &Path) -> Result<LlmPipeline, CleanerError> {
    openvino_genai::load().map_err(|e| {
        warn!("OpenVINO GenAI library not found: {e}");
        CleanerError::LibraryMissing
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
        CleanerError::DeviceFailed
    })
}

fn generate(pipeline: &mut LlmPipeline, prompt: &str) -> Result<String, CleanerError> {
    let mut config = GenerationConfig::new().map_err(generate_failed)?;
    config
        .set_max_new_tokens(MIN_RESPONSE_LEN)
        .map_err(generate_failed)?;
    let text = pipeline
        .generate(prompt, Some(&config), None)
        .and_then(|results| results.get_string())
        .map_err(generate_failed)?;
    if text.trim().is_empty() {
        return Err(CleanerError::EmptyOutput);
    }
    Ok(text)
}

fn generate_failed(e: impl fmt::Display) -> CleanerError {
    error!("OpenVINO generation failed: {e}");
    CleanerError::GenerateFailed
}

#[tauri::command]
#[specta::specta]
pub async fn prepare_openvino_cleaner(
    app: AppHandle,
    cleaner: State<'_, OpenVinoCleaner>,
) -> Result<(), CleanerError> {
    cleaner.prepare(&settings::get_settings(&app)).await
}

#[tauri::command]
#[specta::specta]
pub async fn download_openvino_model(
    app: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: String,
) -> Result<(), String> {
    let model = offered_model(&model_id).ok_or_else(|| CleanerError::UnknownModel.to_string())?;
    model_manager
        .download_hf_snapshot(
            model.id,
            model.label,
            model.repo,
            model.revision,
            model.files,
        )
        .await
        .map(|_| ())
        .map_err(|e| {
            let error = e.to_string();
            error!("OpenVINO model download failed for {}: {e:#}", model.id);
            let _ = app.emit(
                "model-download-failed",
                serde_json::json!({ "model_id": model.id, "error": &error }),
            );
            error
        })
}

#[tauri::command]
#[specta::specta]
pub fn change_openvino_device_setting(
    app: AppHandle,
    device: OpenVinoDevice,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.post_process_openvino_device = device;
    settings::write_settings(&app, settings);
    Ok(())
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum CleanerError {
    UnknownModel,
    ModelNotDownloaded,
    LibraryMissing,
    DeviceFailed,
    GenerateFailed,
    EmptyOutput,
    PromptTooLong,
}

impl fmt::Display for CleanerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CleanerError::UnknownModel => "the selected model is not offered",
            CleanerError::ModelNotDownloaded => "the model is not downloaded",
            CleanerError::LibraryMissing => "the OpenVINO GenAI library was not found",
            CleanerError::DeviceFailed => "the model could not be loaded on the device",
            CleanerError::GenerateFailed => "generation failed",
            CleanerError::EmptyOutput => "the model returned no text",
            CleanerError::PromptTooLong => "the prompt is too long for the NPU",
        })
    }
}

/// The transcript replaces every `${output}`. A template without one still
/// gets the transcript, on its own line at the end.
fn fill_prompt(template: &str, transcription: &str) -> String {
    if template.contains("${output}") {
        template.replace("${output}", transcription)
    } else {
        format!("{template}\n{transcription}")
    }
}

fn check_prompt_budget(device: OpenVinoDevice, prompt: &str) -> Result<(), CleanerError> {
    if device == OpenVinoDevice::Npu && prompt.chars().count() > MAX_PROMPT_LEN * CHARS_PER_TOKEN {
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
    fn placeholder_is_replaced_by_the_transcript() {
        let prompt = fill_prompt("<t>${output}</t> again ${output}", "hello there");
        assert!(prompt.contains("hello there"));
        assert!(!prompt.contains("${output}"));
    }

    #[test]
    fn template_without_placeholder_still_sees_the_transcript() {
        let prompt = fill_prompt("Clean this up.", "hello there");
        assert!(prompt.contains("Clean this up."));
        assert!(prompt.contains("hello there"));
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
    fn a_prompt_over_the_npu_budget_is_too_long() {
        let long = "a".repeat(4097);
        assert_eq!(
            check_prompt_budget(OpenVinoDevice::Npu, &long),
            Err(CleanerError::PromptTooLong)
        );
        assert_eq!(
            check_prompt_budget(OpenVinoDevice::Npu, &"a".repeat(4096)),
            Ok(())
        );
        assert_eq!(check_prompt_budget(OpenVinoDevice::Cpu, &long), Ok(()));
        assert_eq!(check_prompt_budget(OpenVinoDevice::Gpu, &long), Ok(()));
    }
}
