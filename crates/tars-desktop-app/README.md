The Tauri shell for the TARS debug GUI (Doc 22) — loads config, builds the tars-desktop `Backend`, and exposes it as `invoke`-able commands; CI-excluded (needs a system webview).

- Role (hex): shell (Tauri/webview driving adapter; a bin, not a library)
- Effect budget: process/GUI (webview window, Tauri event emission to the frontend) — all domain effects are the Backend's, reached through tars-desktop
- Deps: may depend on [tars-desktop (ALL real work lives there — the split exists so logic stays CI-testable), tars-config, tauri, tokio]; MUST NOT import [tars-server/tars-runtime/tars-pipeline/tars-storage directly → consume them through tars-desktop's Backend facade, or the CI-testability split dies; rusqlite/reqwest → owning layers]
- Owns concepts: [the `#[tauri::command]` surface, AppBackend managed state, tauri.conf.json/capabilities, the static frontend assets]
- Reason to change (the ONE): the Tauri wiring changes (a command binding, window config, frontend asset)
- Belongs here: a new `#[tauri::command]` that forwards to an existing Backend method; window/permission config; frontend JS/HTML
- Does NOT belong: any logic beyond forward-and-serialize → tars-desktop Backend; a new GUI capability's implementation → tars-desktop first, command binding here second; telemetry init policy → tars-melt
