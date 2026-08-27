The TARS-native backend core for the desktop debug GUI (Doc 22) — pure Rust, headlessly CI-testable; the Tauri shell (tars-desktop-app) is a thin wrapper exposing these methods as commands.

- Role (hex): composition-root (assembles tars-server AppState + tars-runtime Session + tars-storage event log into one `Backend` facade for the GUI)
- Effect budget: none directly — LLM via pipeline/AppState, chat via Session, persistence via tars_storage::open_agent_event_log_at_path; fs only through dirs for default paths
- Deps: may depend on [tars-server (AppState reuse), tars-runtime (Session), tars-config, tars-pipeline, tars-storage, tars-types, dirs, tokio]; MUST NOT import [tauri → tars-desktop-app owns the webview shell (the whole point of the split is that THIS crate builds on the CI runner); rusqlite → tars-storage; reqwest → tars-provider]
- Owns concepts: [Backend, ProviderInfo, ChatTurn, ChatMsgView, ConversationMeta, TrajectorySummary, per-turn GUI parameter mapping]
- Reason to change (the ONE): what the debug GUI can DO changes (a new panel capability, a new backend method)
- Belongs here: a new Backend method composing existing layers; conversation-history bookkeeping; view-model (Serialize) structs for the frontend
- Does NOT belong: `#[tauri::command]` wrappers / window / event emission → tars-desktop-app; session semantics → tars-runtime; HTTP endpoints → tars-server
