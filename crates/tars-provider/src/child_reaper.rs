//! Process-wide registry of spawned subprocess PIDs + a best-effort
//! [`kill_all_spawned`] reaper.
//!
//! Why this exists: every claude backend spawns its child with
//! `kill_on_drop(true)`, which only fires on a graceful `Drop` (normal
//! return or panic-unwind). When the *host* process is terminated by a
//! signal (SIGINT from Ctrl-C, or SIGTERM/kill) it dies WITHOUT unwinding,
//! so those `Child` handles never drop and the claude subprocesses — and,
//! for the SDK backend, the long-lived Node daemon plus ITS claude
//! children — orphan. They then run for days, eating the Max-subscription
//! concurrency wall.
//!
//! This module is the *mechanism* only. A library must not grab the
//! process's signal handler — the host application installs the handler
//! and calls [`kill_all_spawned`] from it. Spawn sites register their
//! child's PID here right after a successful spawn (and deregister when the
//! child is awaited / killed / dropped on the graceful path).
//!
//! Known limitation: SIGKILL of the host process itself is uncatchable, so
//! no userspace handler can run then. Only SIGINT / SIGTERM (the realistic
//! Ctrl-C / `kill` case) are recoverable this way.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

/// PIDs of subprocesses spawned by this process that are still live (as far
/// as we know). Each spawn site registers its child here and deregisters on
/// the graceful teardown path.
static SPAWNED: LazyLock<Mutex<HashSet<u32>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Record a freshly-spawned child PID so a signal-time reaper can find it.
///
/// Call this immediately after a successful `.spawn()`. Spawn the child in
/// its OWN process group (`.process_group(0)` on unix) so the negative-PID
/// group kill in [`kill_all_spawned`] tears down the whole subtree.
pub fn register(pid: u32) {
    if let Ok(mut set) = SPAWNED.lock() {
        set.insert(pid);
    }
}

/// Drop a PID from the registry once the child has been awaited / killed /
/// dropped on the graceful path. Idempotent.
pub fn deregister(pid: u32) {
    if let Ok(mut set) = SPAWNED.lock() {
        set.remove(&pid);
    }
}

/// Best-effort reaper: SIGKILL every registered child's *process group* and
/// clear the registry. Intended to be called from the host application's
/// signal handler (SIGINT / SIGTERM) just before exiting.
///
/// On unix each child was spawned as its own group leader, so sending the
/// signal to the negated PID (`-pid`) kills the leader AND every descendant
/// in that group — i.e. the SDK daemon's claude children die too. `ESRCH`
/// (process already gone) is ignored.
///
/// This is intentionally signal-safe-ish: it only takes a `std::sync::Mutex`
/// and makes raw `kill(2)` syscalls. It does no allocation beyond draining
/// the set and no async work.
pub fn kill_all_spawned() {
    let pids: Vec<u32> = match SPAWNED.lock() {
        Ok(mut set) => set.drain().collect(),
        // Poisoned (a holder panicked). Reaping is best-effort and more
        // important than the lock's integrity here, so recover the guard.
        Err(poisoned) => poisoned.into_inner().drain().collect(),
    };

    #[cfg(unix)]
    for pid in pids {
        // `killpg(pgid, SIGKILL)` signals the whole process group led by
        // `pid` (each child was spawned as its own group leader), so the
        // SDK daemon's claude children die too. ESRCH (already gone) and
        // any other error are deliberately ignored — best-effort reaping.
        let pgid = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    }

    #[cfg(not(unix))]
    let _ = pids;
}

/// RAII handle that [`register`]s a PID on construction and [`deregister`]s
/// it on `Drop`. Use it to keep the registry in sync on the graceful path
/// without threading a `deregister` call through every early return /
/// `?`-propagation site. The signal path bypasses `Drop` entirely (that's
/// the whole point of the registry), so the guard is purely for the normal
/// teardown bookkeeping.
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

    // The registry is process-global, so pick PIDs that won't collide with
    // anything a parallel test might register.
    const A: u32 = 4_000_000_001;
    const B: u32 = 4_000_000_002;

    #[test]
    fn register_then_present_deregister_then_absent() {
        register(A);
        assert!(SPAWNED.lock().unwrap().contains(&A));

        deregister(A);
        assert!(!SPAWNED.lock().unwrap().contains(&A));

        // Deregistering an absent PID is a no-op, not a panic.
        deregister(A);
    }

    #[test]
    fn kill_all_clears_the_registry() {
        register(B);
        assert!(SPAWNED.lock().unwrap().contains(&B));

        // `B` is a synthetic PID that isn't a real process; the kill
        // syscall just gets ESRCH and is ignored. The contract under test
        // is that `kill_all_spawned` drains what it reaped — assert `B`
        // specifically is gone rather than global emptiness: the registry is
        // process-global and cargo runs tests in parallel, so another test
        // spawning + `register`ing a real PID between the drain and this check
        // would make an `is_empty()` assertion racily fail (it did).
        kill_all_spawned();
        assert!(!SPAWNED.lock().unwrap().contains(&B));
    }
}
