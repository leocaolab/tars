//! tars-desktop-app — the Tauri shell for the TARS debug GUI (Doc 22).
//!
//! Thin: it loads config, builds the TARS-native [`Backend`] (from the
//! `tars-desktop` core crate), and exposes two commands the static frontend
//! calls via `window.__TAURI__.core.invoke`. All the real work — pipelines,
//! the chat `Session`, telemetry — lives in `tars-desktop` (which is unit
//! tested in CI; this shell isn't, since it needs a system webview).

// On Windows, hide the console window for a release build.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tars_desktop::{Backend, ChatParams, ChatTurn, ProviderInfo};

/// Managed Tauri state: the one backend the whole app drives.
struct AppBackend(Backend);

#[tauri::command]
async fn list_providers(
    backend: tauri::State<'_, AppBackend>,
) -> Result<Vec<ProviderInfo>, String> {
    Ok(backend.0.providers())
}

#[tauri::command]
async fn send_chat(
    backend: tauri::State<'_, AppBackend>,
    provider: Option<String>,
    model: Option<String>,
    system: Option<String>,
    max_output_tokens: Option<u32>,
    user_text: String,
) -> Result<ChatTurn, String> {
    let params = ChatParams {
        system: system.filter(|s| !s.trim().is_empty()),
        max_output_tokens,
    };
    backend
        .0
        .send_once(provider.as_deref(), model.as_deref(), &params, &user_text)
        .await
        .map_err(|e| e.to_string())
}

/// Load `~/.tars/config.toml` if present, else the built-in provider defaults.
fn load_backend() -> anyhow::Result<Backend> {
    let config = match tars_config::default_config_path() {
        Some(path) if path.exists() => tars_config::ConfigManager::load_from_file(&path)?,
        _ => tars_config::ConfigManager::load_from_str("")?,
    };
    Backend::from_config(&config)
}

fn main() {
    let backend = load_backend().expect("failed to build the TARS backend");
    tauri::Builder::default()
        .manage(AppBackend(backend))
        .invoke_handler(tauri::generate_handler![list_providers, send_chat])
        .run(tauri::generate_context!())
        .expect("error while running tars-desktop");
}
