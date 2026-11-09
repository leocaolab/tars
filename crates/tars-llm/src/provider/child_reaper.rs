//! Keeps the PIDs of the subprocesses this process spawned, so the host can kill them when it
//! dies by signal. `kill_on_drop` covers a normal return or a panic, but a host killed by SIGINT
//! or SIGTERM does not unwind, and its CLI children (and the SDK daemon with its own children)
//! would run on for days, holding the subscription's concurrency.
//!
//! It installs no signal handler: the host does, and calls [`kill_all_spawned`] from it. Nothing
//! can run when the host is SIGKILLed.
//!
//! The set belongs to the process, not to a registry or a provider: a signal ends the whole
//! process, so the handler has to reach every child whoever spawned it, and a set handed down by
//! the host would miss the children of any provider built without it.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

/// Holds the PIDs of spawned subprocesses not yet torn down.
static SPAWNED: LazyLock<Mutex<HashSet<u32>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Records a child's PID, right after its spawn. The child must lead its own process group
/// (`.process_group(0)` on unix), since [`kill_all_spawned`] kills by group. A poisoned registry
/// records nothing.
pub fn register(pid: u32) {
    if let Ok(mut set) = SPAWNED.lock() {
        set.insert(pid);
    }
}

/// Removes a PID once its child was awaited, killed or dropped; an absent PID is ignored.
pub fn deregister(pid: u32) {
    if let Ok(mut set) = SPAWNED.lock() {
        set.remove(&pid);
    }
}

/// SIGKILLs, on unix, the process group of every registered child, so their descendants die too,
/// and empties the registry; a failed kill is ignored, and elsewhere nothing is killed. The host
/// calls it on SIGINT or SIGTERM before exiting. It takes a mutex and allocates, so it belongs in a
/// signal-handling task, not in a raw signal handler.
pub fn kill_all_spawned() {
    let pids: Vec<u32> = match SPAWNED.lock() {
        Ok(mut set) => set.drain().collect(),
        // Reaping matters more here than whatever the panicking holder left half done.
        Err(poisoned) => poisoned.into_inner().drain().collect(),
    };

    #[cfg(unix)]
    for pid in pids {
        let pgid = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    }

    #[cfg(not(unix))]
    let _ = pids;
}

/// SIGKILLs, on unix, the process group led by `pid`, so a child's descendants die with it; a
/// group with no process left is not an error. Elsewhere nothing is killed.
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let Ok(raw) = i32::try_from(pid) else {
            tracing::warn!(
                pid,
                "child reaper: a PID past i32::MAX names no process group"
            );
            return;
        };
        match nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(raw),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(e) => {
                tracing::debug!(pid, error = %e, "child reaper: killing a process group failed")
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// A registration that [`register`]s a PID when made and [`deregister`]s it when dropped, so
/// every early return of a spawn site keeps the registry right.
pub struct ReaperGuard {
    pid: u32,
}

impl ReaperGuard {
    pub fn new(pid: u32) -> Self {
        register(pid);
        Self { pid }
    }
}

impl Drop for ReaperGuard {
    fn drop(&mut self) {
        deregister(self.pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-global; these PIDs collide with no real one.
    const A: u32 = 4_000_000_001;
    const B: u32 = 4_000_000_002;

    #[test]
    fn register_then_present_deregister_then_absent() {
        register(A);
        assert!(SPAWNED.lock().unwrap().contains(&A));

        deregister(A);
        assert!(!SPAWNED.lock().unwrap().contains(&A));

        deregister(A);
    }

    #[test]
    fn kill_all_clears_the_registry() {
        register(B);
        assert!(SPAWNED.lock().unwrap().contains(&B));

        // Checks `B` alone, not emptiness: a test running in parallel may register a real PID
        // in between.
        kill_all_spawned();
        assert!(!SPAWNED.lock().unwrap().contains(&B));
    }
}
