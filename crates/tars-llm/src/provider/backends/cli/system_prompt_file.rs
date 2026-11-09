//! Writes a system prompt to a file of its own for `--system-prompt-file`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A system prompt handed to `claude` as a file (`--system-prompt-file`), never as an
/// argv token: Linux caps ONE argument at 128 KiB (`MAX_ARG_STRLEN`), and a system
/// prompt that carries a whole code change passes it — `--system-prompt <text>` then
/// failed the spawn itself (`Argument list too long`, E2BIG) before claude ran.
///
/// Written under the user cache dir (`~/.cache/tars/system-prompts`), not `/tmp`: the
/// delegate's jail binds `/` read-only but mounts a private `/tmp`, so only a path
/// outside `/tmp` is readable in every sandbox mode. Removed when the last invocation
/// holding it is dropped — after the child has exited.
#[derive(Debug)]
pub struct SystemPromptFile {
    path: PathBuf,
}

impl SystemPromptFile {
    /// Writes `text` to a new file of its own under the user cache directory.
    ///
    /// # Errors
    ///
    /// Fails when there is no user cache directory, or the directory or file cannot be written.
    pub fn write(text: String) -> std::io::Result<Self> {
        Self::write_in(&system_prompt_dir()?, text)
    }

    /// Writes `text` to a new file of its own in `dir`.
    ///
    /// # Errors
    ///
    /// Fails when the directory or the file cannot be written.
    pub fn write_in(dir: &Path, text: String) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(unique_name());
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SystemPromptFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::warn!(path = %self.path.display(), error = %e, "a system prompt file was left behind");
        }
    }
}

fn system_prompt_dir() -> std::io::Result<PathBuf> {
    let cache = dirs::cache_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no user cache directory to hold the system prompt",
        )
    })?;
    Ok(cache.join("tars").join("system-prompts"))
}

/// Returns a file name no other prompt of any process holds: the pid, then a counter this
/// process never repeats.
fn unique_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}.md",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
