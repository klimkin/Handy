use crate::managers::stats::{requested_config, StatsManager, StatsSummary};
use crate::settings::get_settings;
use std::sync::Arc;
use tauri::{AppHandle, State};

/// Every group of runs with its box plot figures, plus the settings in use
/// now, which each group's settings are compared with.
#[tauri::command]
#[specta::specta]
pub async fn get_stats_summary(
    app: AppHandle,
    stats: State<'_, Arc<StatsManager>>,
) -> Result<StatsSummary, String> {
    let settings = get_settings(&app);
    let current = requested_config(&settings, &settings.selected_model, false);
    let stats = Arc::clone(&stats);
    tauri::async_runtime::spawn_blocking(move || stats.summary(current))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn clear_stats(stats: State<'_, Arc<StatsManager>>) -> Result<(), String> {
    let stats = Arc::clone(&stats);
    tauri::async_runtime::spawn_blocking(move || stats.clear())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}
