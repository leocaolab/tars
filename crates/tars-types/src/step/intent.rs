//! Defines [`Intent`], the half of a goal that says what to do. The framework carries it
//! without looking inside; the acting step downcasts it to its own type. It is a trait
//! object rather than an associated type on `Step`, which would break `Arc<dyn Step>`.
use std::any::Any;

/// The payload naming what a pursuit is for; a step recovers its own type with
/// `intent.as_any().downcast_ref::<MyIntent>()`.
pub trait Intent: std::fmt::Debug + Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

/// A bare-string intent for simple steps and tests.
#[derive(Debug, Clone)]
pub struct StrIntent(pub String);

impl Intent for StrIntent {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Hands what an upstream step produced on as the next step's intent; a flow streams each
/// stage's [`Refs`](crate::Refs) to the next stage this way.
impl Intent for crate::Refs {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A JSON intent; a spawned child's persisted `intent` rehydrates into it, so every
/// parent-to-child hand-off passes through this type.
#[derive(Debug, Clone)]
pub struct JsonIntent(pub serde_json::Value);

impl Intent for JsonIntent {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The content of one model call: what to say this time.
///
/// It carries no schema, sampling settings or model. Those belong to the step that places
/// the call, because they are the same for every pursuit it is handed ("this critic always
/// runs at temperature 0"); only the content varies per pursuit.
///
/// The content is already rendered, a system string and wire messages rather than a
/// prompt tree: fitting a tree to a window is the assembler's decision, and a step that
/// took a tree would have to know how prompts are composed.
#[derive(Debug, Clone)]
pub struct ChatIntent {
    pub system: Option<String>,
    pub messages: Vec<crate::Message>,
}

impl ChatIntent {
    /// Returns an intent of one user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self::from(crate::ChatRequest::user(text))
    }

    /// Sets the system preface.
    ///
    /// The system text is content, so it rides on the intent; a step with a system slot
    /// of its own would be a second place to say it and need a rule to merge the two.
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }
}

/// Keeps the system text and messages of a rendered request and drops everything else
/// (tools, output schema, sampling settings), so a rendered prompt such as
/// `llm.render_prompt(&tree)` becomes a goal.
impl From<crate::ChatRequest> for ChatIntent {
    fn from(req: crate::ChatRequest) -> Self {
        Self {
            system: req.system,
            messages: req.messages,
        }
    }
}

impl Intent for ChatIntent {
    fn as_any(&self) -> &dyn Any {
        self
    }
}
