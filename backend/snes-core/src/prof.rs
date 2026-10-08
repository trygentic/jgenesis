//! mmxrs patch: optional wall-clock accounting for the widescreen margin
//! renderer. Off (one atomic load per line) until the host installs a clock;
//! the core stays clock-free (wasm). Read by `mmx_core::profile`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static CLOCK: OnceLock<fn() -> u64> = OnceLock::new();
/// Nanoseconds spent in `render_wide_margins` since the last `take_wide_ns`.
static WIDE_NS: AtomicU64 = AtomicU64::new(0);

/// Install a monotonic nanosecond clock (once per process).
pub fn set_clock(f: fn() -> u64) {
    let _ = CLOCK.set(f);
}

#[inline]
#[must_use]
pub fn now() -> Option<u64> {
    CLOCK.get().map(|f| f())
}

pub(crate) fn add_wide_ns(ns: u64) {
    WIDE_NS.fetch_add(ns, Ordering::Relaxed);
}

pub fn take_wide_ns() -> u64 {
    WIDE_NS.swap(0, Ordering::Relaxed)
}
