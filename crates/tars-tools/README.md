Tool trait + ToolRegistry + built-in tools (Doc 05) — the executable side of tool calling: dispatch one ToolCall into one Message::Tool; it does NOT own the agent loop.

- Role (hex): port (Tool, ToolRegistry, approval/permission contracts) + adapter(builtins — fs/process/web effects); the split is folder-clean: src/*.rs is the port, src/builtins/ is the adapters
- Effect budget: fs (read/edit/write/glob/grep/list builtins; linked-in ripgrep/fd engines, no shelling out) | process (BashTool spawns via tokio::process, sandbox-wrapped) | network (web.fetch / web.search via sisurf-core ONLY — no raw HTTP client here)
- Deps: may depend on [tars-types, tars-sandbox (BashTool wraps spawns in SandboxPolicy), sisurf-core (owns the web engine; builtins are thin adapters over its public API), grep/ignore/globset, tokio]; MUST NOT import [reqwest → web capability goes through sisurf-core, LLM HTTP through tars-provider; tars-pipeline/tars-runtime → the loop calls tools, tools never call the loop; rusqlite → the three store owners]
- Owns concepts: [Tool, ToolContext, ToolError, ToolResult, ToolRegistry, ApprovalRequest/ApprovalSink/ApprovalDecision, PermissionView/ToolDecision, builtins::{BashTool, ReadFileTool, EditFileTool, WriteFileTool, GlobTool, GrepTool, ListDirTool, web tools}]
- Reason to change (the ONE): the tool-execution contract changes, or a builtin capability is added/changed
- Belongs here: a new builtin tool module under src/builtins/; a registry dispatch rule; an approval-gate contract
- Does NOT belong: the call-LLM→execute→re-call conversation loop → tars-runtime (lib.rs states this ban); sandbox profile construction → tars-sandbox (re-exported here, not defined here); fetch/search engine internals → sisurf-core
