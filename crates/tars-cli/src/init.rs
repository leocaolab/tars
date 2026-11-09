//! Runs `tars init`: writes a starter config, every provider block in it commented out, to
//! [`tars_config::default_config_path`], the file the commands that call providers read. It refuses to overwrite an existing file without `--force`.

use std::path::{Path, PathBuf};

use clap::Args;

use tars_config::default_config_path;

const STARTER_TEMPLATE: &str = r#"# tars provider registry — user-level.
#
# The `tars` commands that call providers read this file unless --config or
# $TARS_CONFIG names another, and so does tars-py's `Pipeline.from_default(...)`;
# pass an explicit path to `Pipeline.from_config(path, provider_id)` to read a different one.
#
# Schema reference: the "Bootstrap config" section of the tars user guide.
# Auth shape:
#   auth = { kind = "none" }
#   auth = { kind = "delegate" }
#   auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }

# ── Local servers (uncomment / edit to taste) ─────────────────────

# LM Studio (default 127.0.0.1:1234) — most flexible local option;
# `lms load <model>` swaps in any GGUF you've downloaded.
# [providers.lmstudio_local]
# type = "openai_compat"
# gen_ai_provider_name = "lm_studio"   # who serves it, for tracing (OpenTelemetry's name if it has one)
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
#
# default_model is a concrete model id, or `<series>@latest` — the newest
# model of that series, picked from the list `tars models update` refreshes
# into models.json next to this file (run it on a schedule). Gemini series:
# flash, flash-lite, pro. Built-ins `openai`, `anthropic`, `gemini`
# (flash@latest) and `deepseek` (deepseek-flash) need no block here.

# [providers.anthropic_main]
# type = "anthropic"
# default_model = "claude-sonnet-5"
# auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }
#
# [providers.openai_main]
# type = "openai"
# default_model = "gpt-5.4"
# auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
#
# [providers.gemini_flash]
# type = "gemini"
# default_model = "flash@latest"
# auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
#
# [providers.gemini_pro]
# type = "gemini"
# default_model = "pro@latest"
# auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
"#;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Overwrite an existing config file.
    #[arg(long)]
    pub force: bool,

    /// Write to this path instead of the default
    /// `$TARS_HOME/config.toml` (`~/.tars/config.toml` without it).
    #[arg(long)]
    pub path: Option<PathBuf>,
}

/// Why `tars init` wrote no starter config.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("cannot resolve the tars home (set $TARS_HOME or HOME)")]
    NoHome,
    #[error("init filesystem task did not complete (panicked or was cancelled)")]
    Task(#[source] tokio::task::JoinError),
    #[error("creating parent directory {}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{} is a symlink; refusing to overwrite (could clobber its target). \
         Remove it first if you really want to write here.",
        path.display()
    )]
    Symlink { path: PathBuf },
    #[error("inspecting {} before overwrite", path.display())]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} already exists; pass --force to overwrite", path.display())]
    Exists { path: PathBuf },
    #[error("writing starter config to {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub async fn execute(args: InitArgs) -> Result<(), InitError> {
    let target = match args.path {
        Some(p) => p,
        None => default_config_path().ok_or(InitError::NoHome)?,
    };

    let force = args.force;
    let target = tokio::task::spawn_blocking(move || -> Result<PathBuf, InitError> {
        write_starter_config(&target, force)?;
        Ok(target)
    })
    .await
    .map_err(InitError::Task)??;

    print_next_steps(&target);
    Ok(())
}

/// Writes the starter template to `target`, creating its parent directories.
///
/// Without `force` the file is opened `create_new`, so the existence check and the creation are
/// one `open` and a symlink planted at `target` is not followed. With `force` an existing symlink is
/// refused, though a link planted between the check and the open is still followed.
fn write_starter_config(target: &Path, force: bool) -> Result<(), InitError> {
    if let Some(parent) = target.parent() {
        // A bare filename's parent is `Some("")`, which is the cwd.
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| InitError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    let written = |source| InitError::Write {
        path: target.to_path_buf(),
        source,
    };

    if force {
        match std::fs::symlink_metadata(target) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(InitError::Symlink {
                    path: target.to_path_buf(),
                });
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(InitError::Inspect {
                    path: target.to_path_buf(),
                    source,
                });
            }
        }
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(target)
            .map_err(written)?;
        return write_template(f).map_err(written);
    }

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(f) => write_template(f).map_err(written),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(InitError::Exists {
            path: target.to_path_buf(),
        }),
        Err(source) => Err(written(source)),
    }
}

/// Writes the starter template into `f` and syncs it, so a failed write is an error here, not a
/// silently truncated config.
fn write_template(mut f: std::fs::File) -> std::io::Result<()> {
    use std::io::Write;

    f.write_all(STARTER_TEMPLATE.as_bytes())?;
    f.flush()?;
    f.sync_all()
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
    println!("  3. test:  tars probe <id> --tenant <tenant>");
    println!();
    println!("built-in provider ids available without any config:");
    let builtins: Vec<String> = tars_config::built_in_provider_ids()
        .iter()
        .map(|id| id.as_str().to_string())
        .collect();
    println!("  {}", builtins.join(", "));
}
