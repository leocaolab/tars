//! Built-in [`OutputValidator`] implementations shipped with tars.
//!
//! v1 (Doc 15 Wave 1) ships three: `JsonShapeValidator`,
//! `NotEmptyValidator`, `MaxLengthValidator`. These are the
//! deterministic / cheap / generic-mechanism set that 80% of consumers
//! need. Application-specific or business-leaning validators
//! (`RuleIdWhitelistValidator`, `EvidenceTagValidator`,
//! `RegexBannedValidator`) are NOT here — consumers compose them via
//! the trait. See Doc 15 §5 for full philosophy.

use std::collections::HashMap;

use tars_types::{ChatRequest, ChatResponse, ValidationOutcome};

use super::OutputValidator;

// ── JsonShapeValidator ───────────────────────────────────────────────

/// Validates that `response.text` parses as JSON. On parse failure,
/// surfaces `Reject`. Use as the first validator in a chain when
/// downstream validators assume JSON output.
///
/// **Schema validation is shape-only** — v1 uses `serde_json` parsing
/// and doesn't pull a full JSON Schema crate. Future enrichment in
/// B-20.v2 (typed reasons) when the `jsonschema` crate gets added.
pub struct JsonShapeValidator {
    name: String,
}

impl Default for JsonShapeValidator {
    fn default() -> Self {
        Self { name: "json_shape".into() }
    }
}

impl JsonShapeValidator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the validator name (for distinguishing instances in
    /// `ValidationSummary.outcomes`).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

impl OutputValidator for JsonShapeValidator {
    fn name(&self) -> &str {
        &self.name
    }

    fn validate(&self, _req: &ChatRequest, resp: &ChatResponse) -> ValidationOutcome {
        // Empty response is its own concern (use NotEmptyValidator).
        // Empty string passes JSON-parse-as-null but most callers want
        // to flag it via NotEmpty separately, not JsonShape, so we
        // treat empty as Pass here.
        if resp.text.is_empty() {
            return ValidationOutcome::Pass;
        }
        match serde_json::from_str::<serde_json::Value>(resp.text.trim()) {
            Ok(_) => ValidationOutcome::Pass,
            Err(e) => ValidationOutcome::Reject {
                reason: format!("response.text is not valid JSON: {e}"),
            },
        }
    }
}

// ── NotEmptyValidator ────────────────────────────────────────────────

/// Validates that the response carries non-empty content on a chosen
/// field. Defaults to `text`. Useful as a first guard against models
/// that occasionally emit an empty `Finished` event due to safety
/// filters / token cutoff / abort.
pub struct NotEmptyValidator {
    name: String,
    field: ResponseField,
}

#[derive(Debug, Clone, Copy)]
pub enum ResponseField {
    /// Concatenated text content.
    Text,
    /// Reasoning / thinking channel (o1, Qwen3-thinking, etc.).
    Thinking,
}

impl Default for NotEmptyValidator {
    fn default() -> Self {
        Self { name: "not_empty".into(), field: ResponseField::Text }
    }
}

impl NotEmptyValidator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_field(field: ResponseField) -> Self {
        Self { field, ..Self::default() }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

impl OutputValidator for NotEmptyValidator {
    fn name(&self) -> &str {
        &self.name
    }

    fn validate(&self, _req: &ChatRequest, resp: &ChatResponse) -> ValidationOutcome {
        let s = match self.field {
            ResponseField::Text => &resp.text,
            ResponseField::Thinking => &resp.thinking,
        };
        if s.trim().is_empty() {
            ValidationOutcome::Reject {
                reason: format!("response.{} is empty", field_label(self.field)),
            }
        } else {
            ValidationOutcome::Pass
        }
    }
}

fn field_label(f: ResponseField) -> &'static str {
    match f {
        ResponseField::Text => "text",
        ResponseField::Thinking => "thinking",
    }
}

// ── MaxLengthValidator ───────────────────────────────────────────────

/// Validates that a response field doesn't exceed a configured length
/// in characters. Two modes: `Reject` (fail the call permanently) or
/// `Filter` (truncate in-place, downstream sees the shorter version +
/// a `dropped` audit note). Useful for defending against runaway
/// generation, prompt injection causing model to dump training data,
/// or budget-bound callers (chat UI, downstream parser) that can't
/// tolerate huge inputs.
pub struct MaxLengthValidator {
    name: String,
    field: ResponseField,
    max_chars: usize,
    on_exceed: OnExceed,
}

#[derive(Debug, Clone, Copy)]
pub enum OnExceed {
    /// Reject the response permanently — `ValidationFailed` always
    /// surfaces as `ErrorClass::Permanent`; no retry.
    Reject,
    /// Truncate the field in-place to `max_chars`. Subsequent
    /// validators see the truncated response. The dropped tail's
    /// length is recorded in the `Filter.dropped` audit list.
    Truncate,
}

impl MaxLengthValidator {
    /// Construct a Reject-mode validator on `text`.
    pub fn reject_above(max_chars: usize) -> Self {
        Self {
            name: "max_length".into(),
            field: ResponseField::Text,
            max_chars,
            on_exceed: OnExceed::Reject,
        }
    }

    /// Construct a Truncate-mode validator on `text`.
    pub fn truncate_above(max_chars: usize) -> Self {
        Self {
            name: "max_length".into(),
            field: ResponseField::Text,
            max_chars,
            on_exceed: OnExceed::Truncate,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn for_field(mut self, field: ResponseField) -> Self {
        self.field = field;
        self
    }
}

impl OutputValidator for MaxLengthValidator {
    fn name(&self) -> &str {
        &self.name
    }

    fn validate(&self, _req: &ChatRequest, resp: &ChatResponse) -> ValidationOutcome {
        let s = match self.field {
            ResponseField::Text => &resp.text,
            ResponseField::Thinking => &resp.thinking,
        };
        let len = s.chars().count();
        if len <= self.max_chars {
            return ValidationOutcome::Pass;
        }
        match self.on_exceed {
            OnExceed::Reject => ValidationOutcome::Reject {
                reason: format!(
                    "response.{} length={len} exceeds max_chars={}",
                    field_label(self.field),
                    self.max_chars
                ),
            },
            OnExceed::Truncate => {
                let mut new_resp = resp.clone();
                let truncated: String = s.chars().take(self.max_chars).collect();
                let dropped_chars = len - self.max_chars;
                match self.field {
                    ResponseField::Text => new_resp.text = truncated,
                    ResponseField::Thinking => new_resp.thinking = truncated,
                }
                let mut metrics = HashMap::new();
                metrics.insert(
                    "dropped_chars".to_string(),
                    serde_json::Value::from(dropped_chars),
                );
                ValidationOutcome::Filter {
                    response: new_resp,
                    dropped: vec![format!(
                        "{}: truncated {} char(s) past max_chars={}",
                        field_label(self.field),
                        dropped_chars,
                        self.max_chars
                    )],
                }
            }
        }
    }
}
