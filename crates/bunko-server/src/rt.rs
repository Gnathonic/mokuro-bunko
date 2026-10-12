//! The background runtime: the second tokio runtime that `serve` builds for work no
//! request waits on (metadata timers, PROPFIND cache refreshes, thumbnails, the community
//! fetcher, OCR control loops, update checks, sweeps).
//!
//! Requests and background work used to share one runtime and its blocking pool, which
//! is first-in first-out: a burst of background jobs (a regeneration of every series)
//! queued ahead of every request, so even a static 404 waited behind it. With two
//! runtimes the serving one's blocking pool only ever holds request work.
//!
//! Services that run in the background are created and started inside
//! [`enter`]'s context, so the handles they capture (`Handle::try_current`) and the
//! tasks they spawn belong to the background runtime. Without one (tests, tools) the
//! current runtime is used, as before.

use std::sync::OnceLock;
use tokio::runtime::{EnterGuard, Handle};

static BACKGROUND: OnceLock<Handle> = OnceLock::new();

/// Register the background runtime (once, by `serve`). Later calls are ignored.
pub fn set_background(handle: Handle) {
    let _ = BACKGROUND.set(handle);
}

/// The background runtime, if one was registered.
pub fn background() -> Option<Handle> {
    BACKGROUND.get().cloned()
}

/// Enter the background runtime's context for the life of the guard (nothing without
/// one). Hold it only across synchronous code: never across an `.await`.
pub fn enter() -> Option<EnterGuard<'static>> {
    BACKGROUND.get().map(Handle::enter)
}
