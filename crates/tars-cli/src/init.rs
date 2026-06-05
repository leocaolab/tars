//! `tars init` — bootstrap a starter user-level config.
//!
//! Writes a minimal but useful config to the path returned by
//! `tars_config::default_config_path()` (`~/.tars/config.toml`).
//! Refuses to overwrite an existing file without `--force` so users
//! can't accidentally trash their real config.
//!
//! The starter content lists local-server providers (LM Studio, MLX,
//! vLLM) ready to use, plus commented-out templates for the major
//! cloud providers (Anthropic, OpenAI, Gemini). This is the same
//! template a hand-rolled config would use, packaged as a one-liner.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::Args;

use tars_config::default_config_path;

/// Inline starter template. Kept here (vs `include_str!`) so it's
/// reviewable in one place and survives `cargo install` without an
/// extra packaged data file.
const STARTER_TEMPLATE: &str = r#"# tars provider registry — user-level.
#
# Default config path resolved by tars_config::default_config_path().
# Any tars consumer (CLI, downstream Python, scripts, future tools) that calls
# Pipeline::from_default(...) reads this file. Per-project override is
# always available via Pipeline::from_config(explicit_path, provider_id).
#
# Schema reference: see crates/tars-config/src/providers.rs.
# Auth shape:
#   auth = { kind = "none" }
#   auth = { kind = "delegate" }
#   auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }

# ── Local servers (uncomment / edit to taste) ─────────────────────

# LM Studio (default 127.0.0.1:1234) — most flexible local option;
# `lms load <model>` swaps in any GGUF you've downloaded.
# [providers.lmstudio_local]
# type = "openai_compat"
# default_model = "qwen/qwen3-coder-30b"
# base_url = "http://127.0.0.1:1234/v1"

# mlx_lm.server on Apple Silicon (default 127.0.0.1:8080).
# Built-in alias `mlx` already provides this — uncomment only if you
# want to override default_model or base_url.
# [providers.mlx_local]
# type = "mlx"
# default_model = "mlx-community/Qwen2.5-Coder-32B-Instruct-4bit"

# vLLM cluster — see built-in `vllm` alias.

# ── Cloud (uncomment + ensure env vars are set) ────────────────────

# [providers.anthropic_main]
# type = "anthropic"
# default_model = "claude-sonnet-4-7"
# auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }
#
# [providers.openai_main]
# type = "openai"
# default_model = "gpt-4o"
# auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
#
# [providers.gemini_flash]
# type = "gemini"
# default_model = "gemini-2.5-flash"
# auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
"#;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Overwrite an existing config file. Off by default to protect
    /// against accidental clobbering.
    #[arg(long)]
    pub force: bool,

    /// Write to this path instead of the default
    /// `~/.tars/config.toml`. Useful for staging.
    #[arg(long)]
    pub path: Option<PathBuf>,
}

pub async fn execute(args: InitArgs) -> Result<()> {
    let target = match args.path {
        Some(p) => p,
        None => default_config_path()
            .ok_or_else(|| anyhow!("could not resolve default config directory"))?,
    };

    // Filesystem work is blocking; run it on the blocking pool so it
    // can't stall other tasks sharing this runtime. Hand the path back
    // out so we can print next-steps with it.
    let force = args.force;
    let target = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        write_starter_config(&target, force)?;
        Ok(target)
    })
    .await
    .context("init filesystem task did not complete (panicked or was cancelled)")??;

    print_next_steps(&target);
    Ok(())
}

/// Write the starter template to `target`. When `force` is false this
/// uses `create_new` so the existence check and the write are a single
/// atomic syscall — closing the TOCTOU window a separate `exists()` +
/// `write()` would open, and refusing to follow/overwrite a symlink
/// planted at the target path (a plain `fs::write` would clobber the
/// symlink's destination instead).
fn write_starter_config(target: &Path, force: bool) -> Result<()> {
    if let Some(parent) = target.parent() {
        // `parent()` of a bare filename is `Some("")`; skip the empty
        // path so we don't ask the OS to create the cwd.
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directory {}", parent.display()))?;
        }
    }

    use std::io::Write;

    if force {
        // Refuse to follow a symlink planted at the target: a plain
        // `fs::write` (or `OpenOptions::write` without the guard) would
        // clobber the symlink's *destination* — a TOCTOU / symlink-attack
        // vector. `symlink_metadata` does NOT traverse the link, so we
        // can detect and reject it. There's still an inherent race
        // between the check and the open, but rejecting an existing
        // symlink closes the common case; the open below truncates a
        // real file in place.
        match std::fs::symlink_metadata(target) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(anyhow!(
                    "{} is a symlink; refusing to overwrite (could clobber its target). \
                     Remove it first if you really want to write here.",
                    target.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("inspecting {} before overwrite", target.display()));
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(target)
            .with_context(|| format!("opening {} for overwrite", target.display()))?;
        f.write_all(STARTER_TEMPLATE.as_bytes())
            .with_context(|| format!("writing starter config to {}", target.display()))?;
        // Flush + sync so a write/fsync error surfaces here rather than
        // leaving a silently-truncated config behind.
        f.flush()
            .with_context(|| format!("flushing starter config to {}", target.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing starter config to {}", target.display()))?;
        return Ok(());
    }

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(mut f) => {
            f.write_all(STARTER_TEMPLATE.as_bytes())
                .with_context(|| format!("writing starter config to {}", target.display()))?;
            // Surface write/fsync errors before we report success.
            f.flush()
                .with_context(|| format!("flushing starter config to {}", target.display()))?;
            f.sync_all()
                .with_context(|| format!("syncing starter config to {}", target.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
            "{} already exists; pass --force to overwrite",
            target.display()
        )),
        Err(e) => Err(e).with_context(|| format!("writing starter config to {}", target.display())),
    }
}

fn print_next_steps(target: &Path) {
    println!("wrote starter config to {}", target.display());
    println!();
    println!("next steps:");
    println!(
        "  1. open {} and uncomment the providers you use",
        target.display()
    );
    println!("  2. for cloud providers, export the relevant env vars (ANTHROPIC_API_KEY, …)");
    println!("  3. test:  tars run --provider <id> 'hello'");
    println!();
    println!("built-in provider ids available without any config:");
    println!("  openai, anthropic, gemini, claude_cli, gemini_cli, mlx, llamacpp, vllm");
}
