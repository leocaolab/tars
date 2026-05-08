//! Built-in [`OutputValidator`] implementations shipped with tars.
//!
//! v1 (Doc 15 Wave 1) ships three: `JsonShapeValidator`,
//! `NotEmptyValidator`, `MaxLengthValidator`. These are the
//! deterministic / cheap / generic-mechanism set that 80% of consumers
//! need. ARC-specific or business-leaning validators
//! (`RuleIdWhitelistValidator`, `EvidenceTagValidator`,
//! `RegexBannedValidator`) are NOT here — consumers compose them via
//! the trait. See Doc 15 §5 for full philosophy.

use std::collections::HashMap;

use tars_types::{ChatRequest, ChatResponse, ValidationOutcome};

use super::OutputValidator;

// ── JsonShapeValidator ───────────────────────────────────────────────

/// Validates that `response.text` parses as JSON (and optionally
/// matches a schema). On parse failure or schema mismatch, surfaces
/// `Reject`. Use as the first validator in a chain when downstream
/// validators assume JSON output.
///
/// **Schema validation is shape-only** — we use `serde_json` parsing
/// for v1 and don't pull a full JSON Schema crate; the schema parameter
/// here is reserved as a placeholder for B-20 W2 / W3 enrichment when
/// `jsonschema` crate gets added.
///
/// `retriable=true` because parse failures are commonly model
/// non-determinism (skipped a quote, missed a comma) — re-sampling
/// often produces a clean output. Caller can override via
/// `with_retriable(false)` for permanent-shape rejections (caller's
/// schema is wrong).
pub struct JsonShapeValidator {
    name: String,
    retriable_on_fail: bool,
}

impl Default for JsonShapeValidator {
    fn default() -> Self {
        Self {
            name: "json_shape".into(),
            retriable_on_fail: true,
        }
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

    /// Whether to flag rejections as retriable. Default: true (model
    /// non-determinism). Set false when caller knows a clean re-sample
    /// won't help (e.g. response_schema was set but the request
    /// shape was malformed).
    pub fn with_retriable(mut self, retriable: bool) -> Self {
        self.retriable_on_fail = retriable;
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
                retriable: self.retriable_on_fail,
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
    retriable_on_fail: bool,
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
        Self {
            name: "not_empty".into(),
            field: ResponseField::Text,
            retriable_on_fail: true,
        }
    }
}

impl NotEmptyValidator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_field(field: ResponseField) -> Self {
        Self {
            field,
            ..Self::default()
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_retriable(mut self, retriable: bool) -> Self {
        self.retriable_on_fail = retriable;
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
                retriable: self.retriable_on_fail,
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
/// in characters. Two modes: `Reject` (fail the call, RetryMiddleware
/// may re-sample with a tighter budget) or `Filter` (truncate
/// in-place, downstream sees the shorter version + a `dropped` audit
/// note). Useful for defending against runaway generation, prompt
/// injection causing model to dump training data, or budget-bound
/// caller (chat UI, downstream parser) that can't tolerate huge
/// inputs.
pub struct MaxLengthValidator {
    name: String,
    field: ResponseField,
    max_chars: usize,
    on_exceed: OnExceed,
}

#[derive(Debug, Clone, Copy)]
pub enum OnExceed {
    /// Reject the response. Caller / RetryMiddleware decides next.
    Reject { retriable: bool },
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
            on_exceed: OnExceed::Reject { retriable: false },
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
            OnExceed::Reject { retriable } => ValidationOutcome::Reject {
                reason: format!(
                    "response.{} length={len} exceeds max_chars={}",
                    field_label(self.field),
                    self.max_chars
                ),
                retriable,
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
