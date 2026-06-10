//! tars-desktop-app — the Tauri shell for the TARS debug GUI (Doc 22).
//!
//! Thin: it loads config, builds the TARS-native [`Backend`] (from the
//! `tars-desktop` core crate), and exposes commands the static frontend calls
//! via `window.__TAURI__.core.invoke`. All real work — pipelines, conversation
//! history, streaming, telemetry — lives in `tars-desktop` (unit tested in CI;
//! this shell isn't, since it needs a system webview).

// On Windows, hide the console window for a release build.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tars_desktop::{Backend, ChatMsgView, ChatTurn, ConversationMeta, ProviderInfo};
use tauri::Emitter;

/// Managed Tauri state: the one backend the whole app drives.
struct AppBackend(Backend);

#[tauri::command]
async fn list_providers(
    backend: tauri::State<'_, AppBackend>,
) -> Result<Vec<ProviderInfo>, String> {
    Ok(backend.0.providers())
}

#[tauri::command]
async fn new_conversation(
    backend: tauri::State<'_, AppBackend>,
    provider: Option<String>,
    model: Option<String>,
    system: Option<String>,
    max_output_tokens: Option<u32>,
) -> Result<ConversationMeta, String> {
    Ok(backend
        .0
        .new_conversation(provider, model, system, max_output_tokens)
        .await)
}

#[tauri::command]
async fn list_conversations(
    backend: tauri::State<'_, AppBackend>,
) -> Result<Vec<ConversationMeta>, String> {
    Ok(backend.0.list_conversations().await)
}

#[tauri::command]
async fn conversation_messages(
    backend: tauri::State<'_, AppBackend>,
    id: String,
) -> Result<Vec<ChatMsgView>, String> {
    Ok(backend.0.conversation_messages(&id).await)
}

/// Send a turn into a conversation; streams the reply via `chat-delta` events
/// and resolves with the finalized turn (text + metrics).
#[tauri::command]
async fn send_message(
    app: tauri::AppHandle,
    backend: tauri::State<'_, AppBackend>,
    conversation_id: String,
    user_text: String,
) -> Result<ChatTurn, String> {
    let emitter = app.clone();
    backend
        .0
        .stream_in_conversation(&conversation_id, &user_text, move |delta| {
            let _ = emitter.emit("chat-delta", delta.to_string());
        })
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
        .invoke_handler(tauri::generate_handler![
            list_providers,
            new_conversation,
            list_conversations,
            conversation_messages,
            send_message
        ])
        .run(tauri::generate_context!())
        .expect("error while running tars-desktop");
}
