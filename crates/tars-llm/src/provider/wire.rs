//! Writes the wire log: a transport records each attempt it
//! makes — the bytes it sent, the bytes it got back, how the attempt ended —
//! as one `llm_wire` row, at the moment the attempt ends. Only the transport
//! sees those bytes, so only the transport writes them.
//!
//! An attempt is recorded only for a logged call: one whose context carries
//! a [`LoggedCall`](tars_types::LoggedCall), stamped by the call log's emitter.
//! Without one there is no call row for the attempt to belong to.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use bytes::Bytes;
use tars_melt::event::log::LogSink;
use tars_types::{Caller, Millis, ProviderId};
use tars_utils::display_chain;

pub use tars_melt::event::wire::{SpawnedProcess, Transport, WireOutcome, WireRow, WireTable};

/// Where a registry's transports write their wire rows. Writing never
/// waits: a full buffer drops the row and the sink counts it, so its
/// `close()` reports the loss.
#[derive(Clone)]
pub struct WireRecorder {
    sink: Arc<dyn LogSink<WireTable>>,
}

impl WireRecorder {
    pub fn new(sink: Arc<dyn LogSink<WireTable>>) -> Self {
        Self { sink }
    }

    pub fn record(&self, row: WireRow) {
        if let Err(e) = self.sink.try_append(row) {
            tracing::warn!(error = %display_chain(&e), "wire row not logged");
        }
    }

    /// Starts recording an attempt that sends `request_bytes`; `None` when the call is not logged
    /// (no stamp on `ctx`) or the clock cannot say when the attempt started.
    pub(crate) fn begin(
        &self,
        ctx: &Caller,
        provider: &ProviderId,
        model: &str,
        transport: Transport,
        request_bytes: Bytes,
    ) -> Option<Attempt> {
        let stamp = ctx.call.as_ref()?;
        let at_ms = match tars_types::unix_millis(SystemTime::now()) {
            Ok(at_ms) => at_ms,
            Err(e) => {
                tracing::warn!(error = %display_chain(&e), call = %stamp.id, "wire row not logged");
                return None;
            }
        };
        Some(Attempt {
            recorder: self.clone(),
            open: Some(OpenAttempt {
                tenant: ctx.tenant_id.clone(),
                call_id: stamp.id,
                attempt: stamp.next_attempt(),
                at_ms,
                provider_id: provider.clone(),
                model: model.to_owned(),
                transport,
                request_bytes,
                truncated: false,
            }),
            received: ReceivedBytes::default(),
            ended_by_provider: false,
        })
    }
}

impl fmt::Debug for WireRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireRecorder")
            .field("health", &self.sink.health())
            .finish()
    }
}

/// One attempt being recorded. Ended by [`Self::end`]; dropped unended, it
/// records what had arrived, as [`WireOutcome::Completed`] if the provider
/// had already sent its end of the response, else as
/// [`WireOutcome::Cancelled`].
pub(crate) struct Attempt {
    recorder: WireRecorder,
    /// `None` once the row is written.
    open: Option<OpenAttempt>,
    received: ReceivedBytes,
    /// The provider sent its end of the response (the frame that decodes to
    /// `Finished`): whatever the caller does next, the attempt completed.
    ended_by_provider: bool,
}

struct OpenAttempt {
    tenant: tars_types::TenantId,
    call_id: tars_types::CallId,
    attempt: u32,
    at_ms: Millis,
    provider_id: ProviderId,
    model: String,
    transport: Transport,
    request_bytes: Bytes,
    truncated: bool,
}

/// The response bytes as they arrive. Cloned into the byte stream's tap,
/// which runs where the attempt's owner cannot reach.
#[derive(Clone, Default)]
pub(crate) struct ReceivedBytes(Arc<Mutex<Vec<u8>>>);

impl ReceivedBytes {
    pub(crate) fn push(&self, chunk: &[u8]) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(chunk);
    }

    pub(crate) fn take(&self) -> Bytes {
        Bytes::from(std::mem::take(
            &mut *self.0.lock().unwrap_or_else(PoisonError::into_inner),
        ))
    }
}

impl Attempt {
    /// Replaces the request bytes with what was actually sent; a subprocess runner knows what it
    /// wrote into stdin only when it returns.
    #[cfg(feature = "cli")]
    pub(crate) fn sent(&mut self, request_bytes: Bytes) {
        if let Some(open) = self.open.as_mut() {
            open.request_bytes = request_bytes;
        }
    }

    /// Records the HTTP status of the response head; an attempt on another transport has none.
    pub(crate) fn status(&mut self, status: u16) {
        if let Some(OpenAttempt {
            transport: Transport::HttpSse { status: at },
            ..
        }) = self.open.as_mut()
        {
            *at = Some(status);
        }
    }

    /// Returns the buffer the response bytes go into as they arrive.
    pub(crate) fn received(&self) -> ReceivedBytes {
        self.received.clone()
    }

    /// Records the process the subprocess transport spawned; an attempt on another transport
    /// spawns none.
    #[cfg(feature = "cli")]
    pub(crate) fn spawned(&mut self, process: SpawnedProcess) {
        if let Some(OpenAttempt {
            transport: Transport::Subprocess { process: at },
            ..
        }) = self.open.as_mut()
        {
            *at = Some(process);
        }
    }

    /// Records that the provider has sent its end of the response.
    pub(crate) fn ended_by_provider(&mut self) {
        self.ended_by_provider = true;
    }

    /// Records that the transport stopped reading at its cap with bytes left unread.
    pub(crate) fn truncated(&mut self) {
        if let Some(open) = self.open.as_mut() {
            open.truncated = true;
        }
    }

    pub(crate) fn end(mut self, outcome: WireOutcome) {
        self.write(outcome);
    }

    fn write(&mut self, outcome: WireOutcome) {
        let Some(open) = self.open.take() else {
            return;
        };
        self.recorder.record(WireRow {
            tenant: open.tenant,
            call_id: open.call_id,
            attempt: open.attempt,
            at_ms: open.at_ms,
            provider_id: open.provider_id,
            model: open.model,
            transport: open.transport,
            request_bytes: open.request_bytes,
            response_bytes: self.received.take(),
            response_truncated: open.truncated,
            outcome,
        });
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        let outcome = if self.ended_by_provider {
            WireOutcome::Completed
        } else {
            WireOutcome::Cancelled
        };
        self.write(outcome);
    }
}

/// Returns a failed attempt's outcome carrying `error`'s text as raised.
pub(crate) fn failed(error: &impl std::error::Error) -> WireOutcome {
    WireOutcome::Failed {
        error: display_chain(error),
    }
}
