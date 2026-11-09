//! Checks a support-ticket router without knowing the right queue for any ticket.
//!
//! ```text
//! cargo run -p tars-eval --example ticket_router_checks
//! ```
//!
//! Two checks need no golden answer. A `MembershipInvariant` says every routed queue is one the
//! taxonomy has, so a made-up queue is caught on one response. `PrependedHistory`, a
//! `MetamorphicRelation` written here, says an unrelated earlier exchange must not move a ticket
//! to another queue; it compares two responses, so it catches a router that reads the wrong turn
//! even when both queues are valid. It is written by hand because the library's relations only
//! rewrite the last user text, and this one changes the conversation in front of it.
//!
//! The routers are plain functions standing in for a model, so the example runs offline and
//! deterministically; a real eval sends the same requests to an `LlmService`.

use std::sync::Arc;

use tars_eval::{CheckOutcome, CheckRunner, Invariant, MembershipInvariant, MetamorphicRelation};
use tars_types::{
    ChatEvent, ChatRequest, ChatResponse, ChatResponseBuilder, ContentBlock, Message, StopReason,
    Usage,
};

const QUEUES: [&str; 3] = ["billing", "shipping", "account"];

const ROUTE_PREFIX: &str = "route: ";

const TICKETS: [&str; 3] = [
    "I was charged twice for my last invoice.",
    "My package says delivered but nothing arrived.",
    "I want a refund for the broken lamp.",
];

type Router = fn(&ChatRequest) -> ChatResponse;

/// Requires the routed queue to stay the same when an unrelated earlier exchange is put in front
/// of the conversation.
struct PrependedHistory {
    history: Vec<Message>,
}

impl MetamorphicRelation for PrependedHistory {
    fn name(&self) -> &str {
        "prepended_history"
    }

    fn transform(&self, base: &ChatRequest) -> ChatRequest {
        let mut req = base.clone();
        req.messages.splice(0..0, self.history.iter().cloned());
        req
    }

    fn relation_holds(&self, base: &ChatResponse, transformed: &ChatResponse) -> CheckOutcome {
        let before = routed_queue(base);
        let after = routed_queue(transformed);
        if before == after {
            CheckOutcome::pass()
        } else {
            CheckOutcome::fail(format!(
                "routed to `{before}` alone but to `{after}` after the earlier exchange"
            ))
        }
    }
}

/// Returns the queue a router's reply names; a reply without the prefix is returned whole, so
/// the membership check reports it as it was said.
fn routed_queue(response: &ChatResponse) -> String {
    let text = response.text.trim();
    text.strip_prefix(ROUTE_PREFIX).unwrap_or(text).to_string()
}

fn queue_for(text: &str, refund_queue: &'static str) -> &'static str {
    let text = text.to_lowercase();
    if text.contains("refund") {
        refund_queue
    } else if text.contains("invoice") || text.contains("charged") {
        "billing"
    } else if text.contains("package") || text.contains("delivered") {
        "shipping"
    } else {
        "account"
    }
}

fn user_texts(req: &ChatRequest) -> impl DoubleEndedIterator<Item = &str> {
    req.messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.iter().filter_map(ContentBlock::as_text)),
            _ => None,
        })
        .flatten()
}

/// Routes on the latest user text only and sends refunds to billing.
fn latest_turn_router(req: &ChatRequest) -> ChatResponse {
    let latest = user_texts(req)
        .next_back()
        .expect("every request here is built by `ChatRequest::user`, which holds one user text");
    reply(queue_for(latest, "billing"))
}

/// Routes on the first user text of the whole conversation and invents a `refunds` queue.
fn first_turn_router(req: &ChatRequest) -> ChatResponse {
    let first = user_texts(req)
        .next()
        .expect("every request here is built by `ChatRequest::user`, which holds one user text");
    reply(queue_for(first, "refunds"))
}

fn reply(queue: &str) -> ChatResponse {
    let mut builder = ChatResponseBuilder::new();
    builder.apply(ChatEvent::started("example-router"));
    builder.apply(ChatEvent::Delta {
        text: format!("{ROUTE_PREFIX}{queue}"),
    });
    builder.apply(ChatEvent::Finished {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    });
    builder.finish()
}

fn report(check: &str, result: &CheckOutcome) {
    match result {
        CheckOutcome::Held { .. } => println!("    ok    {check}"),
        CheckOutcome::Violated { reason } => println!("    FAIL  {check}: {reason}"),
    }
}

fn main() {
    let invariants: Vec<Arc<dyn Invariant>> = vec![Arc::new(MembershipInvariant::new(
        "queue_in_taxonomy",
        QUEUES,
        |response| vec![routed_queue(response)],
    ))];
    let runner = CheckRunner::new(invariants);
    let relation = PrependedHistory {
        history: vec![
            Message::user_text("Earlier: my package was late last month."),
            Message::assistant_text(format!("{ROUTE_PREFIX}shipping")),
        ],
    };

    let routers: [(&str, Router); 2] = [
        ("latest_turn_router", latest_turn_router),
        ("first_turn_router", first_turn_router),
    ];
    for (router_name, router) in routers {
        println!("{router_name}");
        for ticket in TICKETS {
            let request = ChatRequest::user(ticket);
            let response = router(&request);
            println!("  {ticket:?} -> {}", routed_queue(&response));

            for (invariant, result) in runner.run(&request, &response) {
                report(&invariant, &result);
            }
            let transformed = router(&relation.transform(&request));
            report(
                relation.name(),
                &relation.relation_holds(&response, &transformed),
            );
        }
    }
}
