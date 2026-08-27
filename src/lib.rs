//! sluice: verified camera offload with twin-card cross-checking.
//!
//! Split lib/bin so the engine is drivable from integration tests and the
//! headless harness without an eframe window in the picture.

// The verdict this program issues rests on reading bytes off a device rather
// than out of a cache, on proving two destinations are different physical
// drives, and on keeping a machine awake for twenty minutes. All three are
// Windows APIs with no portable equivalent, and there is a `#[cfg(not(windows))]`
// fallback in `unbuffered.rs` that does *not* bypass the cache -- a placeholder
// for the Linux port in the backlog. Failing here, loudly, is much better than
// building something that looks like sluice and cannot support its own claims.
#[cfg(not(windows))]
compile_error!(
    "sluice is Windows-only: unbuffered reads, physical device identity and the \
     keep-awake guard have no portable equivalent, and a build without them could \
     not justify a format verdict."
);

pub mod engine;
pub mod ui;
