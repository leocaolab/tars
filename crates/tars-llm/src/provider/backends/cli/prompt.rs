//! Turns a request into the one prompt a CLI takes, and bounds a prompt passed as an argument.

use crate::ProviderError;
use crate::provider::error::RequestRefusal;
use tars_types::{ChatRequest, ContentBlock, Message};

/// The longest prompt that travels as one argument. Linux caps one argument at 32 pages, 128 KiB
/// with its terminating NUL (`MAX_ARG_STRLEN`); macOS caps only argv plus the environment, at
/// 1 MiB, so this fits there too.
pub(crate) const MAX_PROMPT_ARG_BYTES: usize = 128 * 1024 - 1;

/// Returns whether a prompt of `len` bytes fits in one argument.
pub(crate) fn fits_in_one_arg(len: usize) -> bool {
    len <= MAX_PROMPT_ARG_BYTES
}

/// Refuses a prompt for `cli` that does not fit in one argument, so it fails as `InvalidRequest`
/// here rather than with `E2BIG` at spawn.
pub(crate) fn check_prompt_arg(prompt: &str, cli: &'static str) -> Result<(), ProviderError> {
    if fits_in_one_arg(prompt.len()) {
        return Ok(());
    }
    Err(ProviderError::InvalidRequest(
        RequestRefusal::PromptTooLongForArgument {
            bytes: prompt.len(),
            limit: MAX_PROMPT_ARG_BYTES,
            cli,
        },
    ))
}

/// Returns the messages as the one prompt `-p` takes, each under a `[role]` line. A message whose
/// text holds a line like `[user]` cannot be told from a turn boundary; a caller that needs an
/// exact multi-turn history uses an HTTP backend (`claude_sdk` flattens the same way).
pub(crate) fn serialize_messages_for_cli(req: &ChatRequest) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        let (role, content) = match m {
            Message::User { content } => ("user", content),
            Message::Assistant { content, .. } => ("assistant", content),
            Message::Tool { content, .. } => ("tool", content),
            Message::System { content } => ("system", content),
        };
        let flat = flatten_blocks(content);
        parts.push(format!("[{role}]\n{flat}"));
    }
    parts.join("\n\n")
}

fn flatten_blocks(blocks: &[ContentBlock]) -> String {
    let mut out: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } => out.push(text.clone()),
            ContentBlock::Image { mime, .. } => {
                // Brackets in the mime would close the placeholder early.
                let safe: String = mime.chars().filter(|c| *c != ']' && *c != '[').collect();
                out.push(format!("[image:{safe}]"));
            }
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_fits_in_one_arg_up_to_128_kib_less_the_nul() {
        assert!(fits_in_one_arg(128 * 1024 - 1));
        assert!(!fits_in_one_arg(128 * 1024));
    }
}
