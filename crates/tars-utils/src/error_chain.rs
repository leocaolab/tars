//! Renders an error with its causes as one line of text, for a boundary that hands a person or
//! a model a string instead of the error itself.
//!
//! Every error in this workspace says only its own layer's context in its `Display` and hangs
//! the cause on `source`, so `to_string()` alone drops every cause.

use std::error::Error;

/// Returns `err`'s message followed by each cause's along the `source` chain, joined by `: `.
pub fn display_chain(err: &dyn Error) -> String {
    std::iter::successors(err.source(), |&cause| cause.source())
        .fold(err.to_string(), |text, cause| format!("{text}: {cause}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("reading the board")]
    struct Outer {
        #[source]
        source: Middle,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("opening board.db")]
    struct Middle {
        #[source]
        source: std::io::Error,
    }

    #[derive(Debug, thiserror::Error)]
    #[error(transparent)]
    struct Forward(Middle);

    fn middle() -> Middle {
        Middle {
            source: std::io::Error::other("disk on fire"),
        }
    }

    #[test]
    fn each_cause_is_written_once_in_order() {
        let err = Outer { source: middle() };
        assert_eq!(
            display_chain(&err),
            "reading the board: opening board.db: disk on fire"
        );
    }

    #[test]
    fn an_error_without_a_source_is_its_message() {
        assert_eq!(
            display_chain(&std::io::Error::other("disk on fire")),
            "disk on fire"
        );
    }

    #[test]
    fn a_transparent_forward_adds_no_layer() {
        assert_eq!(
            display_chain(&Forward(middle())),
            "opening board.db: disk on fire"
        );
    }
}
