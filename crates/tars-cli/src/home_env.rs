//! Reads `$TARS_HOME/.env`, where an operator keeps keys out of the shell, into the environment
//! the commands resolve env-var credentials from. The process environment is never written; a
//! variable the shell sets wins over the file's.

use std::collections::HashMap;
use std::env::VarError;
use std::path::{Path, PathBuf};

use tars_llm::EnvSource;

/// The process environment with `$TARS_HOME/.env` beneath it.
#[derive(Debug, Default)]
pub struct HomeEnv {
    file: HashMap<String, String>,
}

/// Why `$TARS_HOME/.env` was not read. A line that does not parse is named by its number only,
/// since its text may hold a key.
#[derive(Debug, thiserror::Error)]
pub enum HomeEnvError {
    #[error("reading {}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} line {line} does not parse as `KEY=value`", path.display())]
    BadLine { path: PathBuf, line: usize },
    #[error("{} does not parse as `KEY=value` lines", path.display())]
    Unparsable {
        path: PathBuf,
        #[source]
        source: Option<dotenvy::Error>,
    },
}

impl EnvSource for HomeEnv {
    fn var(&self, name: &str) -> Result<String, VarError> {
        shell_over_file(std::env::var(name), self.file.get(name).map(String::as_str))
    }
}

/// Returns the shell's value of a variable when the shell has one, else the file's.
fn shell_over_file(
    shell: Result<String, VarError>,
    file: Option<&str>,
) -> Result<String, VarError> {
    match (shell, file) {
        (Err(VarError::NotPresent), Some(value)) => Ok(value.to_string()),
        (shell, _) => shell,
    }
}

/// Returns the environment of this invocation: the process's, over the tars home's `.env` when
/// there is one. A home that does not resolve has no `.env`.
///
/// # Errors
///
/// Fails when the file is there but cannot be read, or a line of it does not parse.
pub fn load() -> Result<HomeEnv, HomeEnvError> {
    match tars_config::resolve_home(None) {
        Some(home) => load_from(&home.join(".env")),
        None => Ok(HomeEnv::default()),
    }
}

fn load_from(path: &Path) -> Result<HomeEnv, HomeEnvError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HomeEnv::default()),
        Err(source) => {
            return Err(HomeEnvError::Unreadable {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let mut file = HashMap::new();
    for entry in dotenvy::from_read_iter(content.as_bytes()) {
        match entry {
            Ok((key, value)) => {
                file.entry(key).or_insert(value);
            }
            Err(dotenvy::Error::LineParse(text, _)) => {
                return Err(match line_of(content, &text) {
                    Some(line) => HomeEnvError::BadLine {
                        path: path.to_path_buf(),
                        line,
                    },
                    None => HomeEnvError::Unparsable {
                        path: path.to_path_buf(),
                        source: None,
                    },
                });
            }
            Err(dotenvy::Error::Io(source)) => {
                return Err(HomeEnvError::Unreadable {
                    path: path.to_path_buf(),
                    source,
                });
            }
            Err(other) => {
                return Err(HomeEnvError::Unparsable {
                    path: path.to_path_buf(),
                    source: Some(other),
                });
            }
        }
    }
    Ok(HomeEnv { file })
}

/// Returns the 1-based number of the first line of `content` that starts with `entry`, the text
/// of an entry the parser refused.
fn line_of(content: &str, entry: &str) -> Option<usize> {
    let mut start = 0;
    for (index, line) in content.split_inclusive('\n').enumerate() {
        if content[start..].starts_with(entry) {
            return Some(index + 1);
        }
        start += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shells_value_wins_over_the_files() {
        assert_eq!(
            shell_over_file(Ok("shell".into()), Some("file")),
            Ok("shell".to_string())
        );
        assert_eq!(
            shell_over_file(Err(VarError::NotPresent), Some("file")),
            Ok("file".to_string())
        );
        assert_eq!(
            shell_over_file(Err(VarError::NotPresent), None),
            Err(VarError::NotPresent)
        );
    }

    #[test]
    fn a_bad_line_is_named_by_its_number_and_not_its_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "# keys\nGOOD=1\n\nGEMINI_API_KEY=\"sk-secret\n").unwrap();
        let err = load_from(&path).unwrap_err();
        assert!(
            matches!(err, HomeEnvError::BadLine { line: 4, .. }),
            "{err:?}"
        );
        assert!(!tars_utils::display_chain(&err).contains("sk-secret"));
    }

    #[test]
    fn a_missing_file_is_an_empty_one_and_the_first_entry_of_a_key_wins() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_from(&dir.path().join(".env")).unwrap().file.is_empty());
        let path = dir.path().join(".env");
        std::fs::write(&path, "\u{feff}K=first\nK=second\n").unwrap();
        let env = load_from(&path).unwrap();
        assert_eq!(env.file.get("K").map(String::as_str), Some("first"));
    }

    #[test]
    fn line_of_finds_the_line_an_entry_starts_on() {
        assert_eq!(line_of("A=1\nB=\"x\ny\"\nC", "C"), Some(4));
        assert_eq!(line_of("A=1\n", "Z"), None);
    }
}
