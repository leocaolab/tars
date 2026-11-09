//! Listens for the signals that ask `tars` to stop. Once listening, SIGINT and SIGTERM no longer
//! end the process by themselves: the run decides what to tear down and which code to exit with.

use tokio::signal::unix::{Signal, SignalKind, signal};

/// Names a signal that asks the process to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitSignal {
    /// SIGINT, which Ctrl-C sends.
    Interrupt,
    /// SIGTERM, which `kill` and process supervisors send.
    Terminate,
}

impl ExitSignal {
    /// Returns the signal's number, the same on every unix.
    pub(crate) fn number(self) -> u8 {
        match self {
            Self::Interrupt => 2,
            Self::Terminate => 15,
        }
    }
}

/// The process's SIGINT and SIGTERM listeners.
pub(crate) struct ExitSignals {
    interrupt: Signal,
    terminate: Signal,
}

impl ExitSignals {
    /// Starts listening for SIGINT and SIGTERM.
    ///
    /// # Errors
    ///
    /// Returns the OS error when a handler cannot be installed.
    ///
    /// # Panics
    ///
    /// Panics when called outside a tokio runtime.
    pub(crate) fn listen() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Waits for the first SIGINT or SIGTERM to arrive.
    pub(crate) async fn recv(&mut self) -> ExitSignal {
        tokio::select! {
            Some(()) = self.interrupt.recv() => ExitSignal::Interrupt,
            Some(()) = self.terminate.recv() => ExitSignal::Terminate,
            // Both streams end only when the runtime shuts down, which no signal then needs to
            // interrupt.
            else => std::future::pending().await,
        }
    }
}
