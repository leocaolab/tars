Module placement contract — `tars_tools::builtins` (the effectful adapters).

This folder is a DIFFERENT hex role from the crate root: src/*.rs (tool.rs, registry.rs, approval.rs, permission.rs) is the pure port — traits, dispatch, gates, zero I/O. This folder is where ALL of the crate's effects live: fs (read_file/edit_file/write_file/glob/grep/list_dir), process (bash — tokio::process::Command, sandbox-wrapped, fail-closed), network (web — via sisurf_core only).

- Belongs here: a new builtin = one new module + one `pub use` in mod.rs; path-jail/canonicalize logic for a builtin; a builtin's arg-schema + execute
- Does NOT belong: a change to the Tool trait or ToolContext → crate root src/tool.rs; approval/permission policy → src/approval.rs / src/permission.rs; a raw reqwest call → nowhere in this crate (web goes through sisurf-core; LLM HTTP is tars-provider's)
- Rule of thumb: if a diff in the crate root adds `std::fs` / `Command` / an HTTP client, it is misplaced — effects enter this crate only through this folder.
