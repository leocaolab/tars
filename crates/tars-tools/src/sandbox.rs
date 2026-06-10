//! [`SandboxPolicy`] — the OS-confinement seam (Doc 22 T2).
//!
//! A field on [`ToolContext`](crate::ToolContext) that a future sandboxed `exec`
//! (lifted from Codex's Seatbelt/Landlock crates) reads to confine a subprocess.
//! Today it's a stub with an **unrestricted** default, so tools that ignore it
//! behave exactly as before — the type exists now to keep the `Tool` /
//! `ToolContext` contract stable when the real lift lands.

use std::path::PathBuf;

/// What a tool's side effects are confined to. `Default` = unrestricted (no
/// confinement = current behaviour). Real enforcement (writable-root jail,
/// network policy) is Doc 22 T2.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    /// Directories writes are allowed under. Empty = unrestricted (today's
    /// behaviour); a future sandboxed exec jails writes to these.
    pub writable_roots: Vec<PathBuf>,
    /// Whether network egress is permitted. `true` (default) = unrestricted.
    pub network: bool,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        // Unrestricted: preserves today's behaviour until the lift confines it.
        Self {
            writable_roots: Vec::new(),
            network: true,
        }
    }
}

impl SandboxPolicy {
    /// The explicit unrestricted policy (same as `default()`), for readability
    /// at call sites that want to say "no sandbox yet" out loud.
    pub fn unrestricted() -> Self {
        Self::default()
    }
}
