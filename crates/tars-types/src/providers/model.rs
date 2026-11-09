//! Says how much the provider may think ([`ThinkingMode`]).

use serde::{Deserialize, Serialize};

/// The serde form is a storage format, since the call log stores requests and refusal reasons:
/// `{"mode":"off"}`, `{"mode":"auto"}`, `{"mode":"budget","tokens":N}`.
///
/// A request that says nothing about thinking holds no `ThinkingMode` at all (`None` on
/// `ChatRequest::thinking`), and the provider's default applies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "tokens", rename_all = "snake_case")]
pub enum ThinkingMode {
    /// Turns thinking off, explicitly on every provider that has a switch for it.
    Off,
    /// Let the provider auto-decide thinking depth.
    Auto,
    /// Hard cap on thinking tokens (Anthropic-style).
    Budget(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_thinking_mode_is_written_under_its_mode_and_reads_back() {
        let stored = [
            (ThinkingMode::Off, r#"{"mode":"off"}"#),
            (ThinkingMode::Auto, r#"{"mode":"auto"}"#),
            (
                ThinkingMode::Budget(1024),
                r#"{"mode":"budget","tokens":1024}"#,
            ),
        ];
        for (mode, json) in stored {
            assert_eq!(serde_json::to_string(&mode).unwrap(), json, "{mode:?}");
            assert_eq!(serde_json::from_str::<ThinkingMode>(json).unwrap(), mode);
        }
    }
}
