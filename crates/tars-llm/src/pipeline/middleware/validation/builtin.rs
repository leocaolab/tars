//! Provides the generic, deterministic validators; application-specific ones implement
//! [`OutputValidator`] themselves.

use tars_types::{ChatRequest, ChatResponse, ValidationOutcome, ValidationReason};

use super::OutputValidator;

// ── JsonShapeValidator ───────────────────────────────────────────────

/// Rejects a response whose `text` does not parse as JSON, with [`ValidationReason::JsonShape`].
/// It checks syntax only, not a schema. An empty `text` passes; [`NotEmptyValidator`] is the one
/// that rejects it.
pub struct JsonShapeValidator {
    name: String,
}

impl Default for JsonShapeValidator {
    fn default() -> Self {
        Self {
            name: "json_shape".into(),
        }
    }
}

impl JsonShapeValidator {
    pub fn new() -> Self {
        Self::default()
    }
}

impl OutputValidator for JsonShapeValidator {
    fn name(&self) -> &str {
        &self.name
    }

    fn validate(&self, _req: &ChatRequest, resp: &ChatResponse) -> ValidationOutcome {
        if resp.text.is_empty() {
            return ValidationOutcome::Pass;
        }
        match serde_json::from_str::<serde_json::Value>(resp.text.trim()) {
            Ok(_) => ValidationOutcome::Pass,
            Err(e) => ValidationOutcome::Reject {
                reason: ValidationReason::JsonShape {
                    parse_error: e.to_string(),
                },
            },
        }
    }
}

// ── NotEmptyValidator ────────────────────────────────────────────────

/// Rejects a response whose `text` is empty or whitespace, which models sometimes return after
/// a safety filter, a token cutoff or an abort.
pub struct NotEmptyValidator {
    name: String,
    field: ResponseField,
}

#[derive(Debug, Clone, Copy)]
pub enum ResponseField {
    Text,
    /// The reasoning channel of thinking models.
    Thinking,
}

impl Default for NotEmptyValidator {
    fn default() -> Self {
        Self {
            name: "not_empty".into(),
            field: ResponseField::Text,
        }
    }
}

impl NotEmptyValidator {
    pub fn new() -> Self {
        Self::default()
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
                reason: ValidationReason::NotEmpty {
                    field: field_label(self.field).to_string(),
                },
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

/// Rejects or truncates a response whose `text` is longer than `max_chars` characters.
pub struct MaxLengthValidator {
    name: String,
    field: ResponseField,
    max_chars: usize,
    on_exceed: OnExceed,
}

#[derive(Debug, Clone, Copy)]
pub enum OnExceed {
    Reject,
    /// Cuts the field to `max_chars` and records the dropped length in `Filter.dropped`; later
    /// validators see the shorter response.
    Truncate,
}

impl MaxLengthValidator {
    /// Returns a validator that rejects a `text` longer than `max_chars`.
    pub fn reject_above(max_chars: usize) -> Self {
        Self {
            name: "max_length".into(),
            field: ResponseField::Text,
            max_chars,
            on_exceed: OnExceed::Reject,
        }
    }

    /// Returns a validator that truncates a `text` longer than `max_chars`.
    pub fn truncate_above(max_chars: usize) -> Self {
        Self {
            name: "max_length".into(),
            field: ResponseField::Text,
            max_chars,
            on_exceed: OnExceed::Truncate,
        }
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
                reason: ValidationReason::MaxLength {
                    field: field_label(self.field).to_string(),
                    length: len,
                    max: self.max_chars,
                },
            },
            OnExceed::Truncate => {
                let mut new_resp = resp.clone();
                let truncated: String = s.chars().take(self.max_chars).collect();
                let dropped_chars = len - self.max_chars;
                match self.field {
                    ResponseField::Text => new_resp.text = truncated,
                    ResponseField::Thinking => new_resp.thinking = truncated,
                }
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
