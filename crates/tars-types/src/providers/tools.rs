//! Defines the tools a request offers the model and the tool calls the model sends back.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::providers::events::StopReason;
use crate::providers::schema::JsonSchema;

/// A tool definition offered to the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The name the model's tool call is matched by, so it must not be blank. The field is
    /// public; build through [`ToolSpec::new`] when the name comes from config or user input, and
    /// check [`has_valid_name`](Self::has_valid_name) at the provider boundary.
    pub name: String,
    /// What the tool is for; the model decides from it when to call the tool, so it says when to
    /// use the tool and not only what it does.
    pub description: String,
    /// The JSON Schema of the arguments object.
    pub input_schema: JsonSchema,
}

impl ToolSpec {
    /// Builds a tool spec.
    ///
    /// # Errors
    ///
    /// Returns [`BlankToolNameError`] when `name` is empty or only whitespace.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: JsonSchema,
    ) -> Result<Self, BlankToolNameError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(BlankToolNameError);
        }
        Ok(Self {
            name,
            description: description.into(),
            input_schema,
        })
    }

    /// Returns whether `name` is not blank; check it before sending `request.tools` to the model.
    pub fn has_valid_name(&self) -> bool {
        !self.name.trim().is_empty()
    }
}

/// A tool name that is empty or only whitespace, which no tool call could be matched by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("ToolSpec.name cannot be empty/whitespace")]
pub struct BlankToolNameError;

/// Which tools the model may or must call.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call a tool.
    #[default]
    Auto,
    /// The model must not call a tool.
    None,
    /// The model must call some tool of its choosing.
    Required,
    /// The model must call the named tool.
    Specific(String),
}

/// One tool call the model emitted. Adapters that receive string-encoded arguments (OpenAI) parse
/// them first, so `arguments` is always the object a tool takes its named arguments from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    /// The provider's id that ties the call to its result message. OpenAI and Anthropic send one;
    /// the Gemini adapter makes one up.
    pub id: String,
    pub name: String,
    #[serde(deserialize_with = "object_arguments")]
    pub arguments: Map<String, Value>,
    /// An opaque provider token to echo back when this call is replayed in a later request.
    /// Gemini's thinking models return a `thoughtSignature` on each functionCall and reject the
    /// next turn without it ("Function call is missing a thought_signature"); `None` for providers
    /// that do not use one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

impl ToolCall {
    /// Builds a tool call with no thought signature.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Map<String, Value>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: None,
        }
    }

    /// Sets [`Self::thought_signature`], replacing any already set.
    pub fn with_thought_signature(mut self, sig: Option<String>) -> Self {
        self.thought_signature = sig;
        self
    }
}

/// The JSON type of a value, as a refusal of tool-call arguments names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonKind {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

impl JsonKind {
    pub fn of(v: &Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Boolean,
            Value::Number(_) => Self::Number,
            Value::String(_) => Self::String,
            Value::Array(_) => Self::Array,
            Value::Object(_) => Self::Object,
        }
    }
}

impl std::fmt::Display for JsonKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        })
    }
}

/// Why the arguments a model sent with a tool call are not the JSON object a tool takes.
#[derive(Debug, thiserror::Error)]
pub enum MalformedArguments {
    /// The text does not parse, an empty prefix included.
    #[error("not JSON")]
    NotJson(#[source] serde_json::Error),
    #[error("a JSON {0}, not an object")]
    NotAnObject(JsonKind),
}

/// How much of a tool call's argument text the stream delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentText {
    /// All the model wrote: the call's block closed, or the stream ended it with its terminator
    /// or a stop reason other than the output limit.
    Whole,
    /// What arrived before the output limit cut the call or the body ended inside it, which may
    /// be a part.
    Prefix,
}

impl ArgumentText {
    /// Returns how much of a call still open arrived when the stream said why the turn stopped:
    /// a part at `MaxTokens`, since the limit is what ended it, and all of it at any other stop.
    pub fn at_stop(stop: StopReason) -> Self {
        match stop {
            StopReason::MaxTokens => Self::Prefix,
            _ => Self::Whole,
        }
    }
}

/// Parses a tool call's argument text into an object. Empty text is a call with no arguments when
/// the text is [`ArgumentText::Whole`], and arguments not received when it is a prefix. Whole text
/// missing its outer braces is retried with them; a prefix is not, since closing it would make a
/// cut call look whole. Text that parses as anything but an object (a string holding the JSON
/// again, an array) is refused: a tool takes named arguments.
///
/// # Errors
///
/// The [`MalformedArguments`] that says why the text is not an object.
pub fn parse_arguments(
    raw: &str,
    text: ArgumentText,
) -> Result<Map<String, Value>, MalformedArguments> {
    let trimmed = raw.trim();
    if trimmed.is_empty() && text == ArgumentText::Whole {
        return Ok(Map::new());
    }
    let parsed = match (serde_json::from_str::<Value>(trimmed), text) {
        (Ok(v), _) => v,
        (Err(first_err), ArgumentText::Whole) => {
            serde_json::from_str::<Value>(&format!("{{{trimmed}}}"))
                .map_err(|_| MalformedArguments::NotJson(first_err))?
        }
        (Err(err), ArgumentText::Prefix) => return Err(MalformedArguments::NotJson(err)),
    };
    arguments_object(parsed)
}

/// Returns `value` as tool-call arguments, or the JSON type it has instead of an object.
///
/// # Errors
///
/// Returns [`MalformedArguments::NotAnObject`] for any value but an object.
pub fn arguments_object(value: Value) -> Result<Map<String, Value>, MalformedArguments> {
    match value {
        Value::Object(map) => Ok(map),
        other => Err(MalformedArguments::NotAnObject(JsonKind::of(&other))),
    }
}

/// Reads recorded tool-call arguments, refusing anything but an object. The refusal names the
/// JSON type and size and not the text, which can hold file contents or user data.
pub(crate) fn object_arguments<'de, D>(deserializer: D) -> Result<Map<String, Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let bytes = value.to_string().len();
    arguments_object(value).map_err(|e| {
        serde::de::Error::custom(format!("tool call arguments are {e} ({bytes} bytes)"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_text_is_no_arguments_when_whole_and_not_json_when_a_prefix() {
        assert!(parse_arguments("", ArgumentText::Whole).unwrap().is_empty());
        assert!(
            parse_arguments("  ", ArgumentText::Whole)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            parse_arguments("", ArgumentText::Prefix),
            Err(MalformedArguments::NotJson(_))
        ));
        assert!(
            parse_arguments("{}", ArgumentText::Prefix)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn whole_argument_text_missing_its_outer_braces_is_retried_with_them() {
        let args = parse_arguments(r#""q":1"#, ArgumentText::Whole).unwrap();
        assert_eq!(Value::Object(args), json!({"q": 1}));
        assert!(matches!(
            parse_arguments("not json", ArgumentText::Whole),
            Err(MalformedArguments::NotJson(_))
        ));
    }

    #[test]
    fn a_prefix_missing_its_outer_braces_is_not_closed() {
        assert!(matches!(
            parse_arguments(r#""path":"a.txt""#, ArgumentText::Prefix),
            Err(MalformedArguments::NotJson(_))
        ));
    }

    #[test]
    fn only_the_output_limit_leaves_an_open_call_a_prefix() {
        assert_eq!(
            ArgumentText::at_stop(StopReason::MaxTokens),
            ArgumentText::Prefix
        );
        for stop in [
            StopReason::EndTurn,
            StopReason::ToolUse,
            StopReason::StopSequence,
            StopReason::ContentFilter,
            StopReason::Cancelled,
            StopReason::Other,
        ] {
            assert_eq!(ArgumentText::at_stop(stop), ArgumentText::Whole, "{stop:?}");
        }
    }

    #[test]
    fn tool_choice_default_is_auto() {
        assert!(matches!(ToolChoice::default(), ToolChoice::Auto));
    }

    #[test]
    fn a_recorded_tool_call_loads_only_when_its_arguments_are_an_object() {
        for bad in [
            r#"{"id":"c","name":"n","arguments":"not-an-object"}"#,
            r#"{"id":"c","name":"n","arguments":[1,2]}"#,
            r#"{"id":"c","name":"n","arguments":42}"#,
        ] {
            assert!(serde_json::from_str::<ToolCall>(bad).is_err(), "{bad}");
        }
        let err = serde_json::from_str::<ToolCall>(
            r#"{"id":"c","name":"n","arguments":"secret file contents"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("secret"), "{err}");
        let ok: ToolCall =
            serde_json::from_str(r#"{"id":"c","name":"n","arguments":{"q":"rust"}}"#).unwrap();
        assert_eq!(ok.arguments.get("q"), Some(&json!("rust")));
    }
}
