//! glibc malloc tuning (honk-9qf.37).
//!
//! By default glibc gives each thread that contends on malloc its own arena (up to 8 per
//! core: 128 on cloudbox) and keeps freed memory inside the arena. honkd's tokio workers
//! each touched large, short-lived allocations (session replays, export rebuilds), so
//! RSS ratcheted to the sum of every arena's high-water mark and never came back: 430 MB
//! resident with one session held, and two OOM kills at a 1 GB cap. honkd is I/O bound
//! (seconds of CPU per day), so one arena costs nothing measurable, and a periodic
//! `malloc_trim` hands freed pages back to the kernel.

use std::time::Duration;

/// How often the trim task returns free heap pages to the kernel.
pub const TRIM_INTERVAL: Duration = Duration::from_secs(60);

/// Cap malloc at one arena. Call before any other thread exists.
pub fn tune() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt only adjusts allocator parameters; called once at startup.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
}

/// Return free pages from the heap to the kernel.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim is thread-safe; it takes each arena's lock while it works.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Spawn the periodic trim task on the current runtime.
pub fn spawn_trim_task() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(TRIM_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            let _ = tokio::task::spawn_blocking(trim).await;
        }
    });
}
