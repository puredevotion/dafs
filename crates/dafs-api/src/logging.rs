//! Runtime log-level control.
//!
//! The daemon installs a reloadable filter layer and hands the handle here, so
//! `GET`/`PUT /log-level` can read and change verbosity without a restart.
//!
//! # Why this exists
//!
//! Reproducing a problem is usually the expensive part of diagnosing one. A
//! daemon that has to be restarted to raise its log level destroys the state
//! that was about to be explained — the scan that is halfway through, the watch
//! that has just stopped reporting. Raising the level against the live process
//! and lowering it again afterwards costs one indirection on a path that is not
//! hot, and it is worth it.
//!
//! # Why it is not a security problem here
//!
//! Changing a log level is a write operation on an unauthenticated API, which
//! would normally be worth refusing. It is acceptable *only* because the API
//! binds loopback (see the daemon's `--listen` default) and so is already
//! restricted to processes on the same machine. If the bind address is ever
//! widened, this endpoint needs authentication along with everything else that
//! is not a read — and the filter string is validated rather than trusted, so
//! widening the bind does not also hand callers a parser to attack.

use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::reload::Handle;

/// The concrete layer type the daemon installs. Named because the reload handle
/// is generic over it and the signature is otherwise unreadable.
type FilterHandle = Handle<EnvFilter, tracing_subscriber::Registry>;

/// Longest accepted filter directive.
///
/// Generous against any real one — `dafs_scan=trace,dafs_store=debug,info` is
/// under 50 bytes — and small enough that the string held resident cannot
/// matter against the daemon's 32 MiB ceiling.
const MAX_FILTER_LEN: usize = 1024;

/// Handle for reading and changing the active log filter.
#[derive(Clone)]
pub struct LogLevelHandle {
    handle: FilterHandle,
    /// The filter string currently in force.
    ///
    /// Kept alongside the handle because `EnvFilter` renders back to a string
    /// that is normalised rather than identical to what was set, and a caller
    /// reading the level back should see what they asked for.
    current: Arc<Mutex<String>>,
}

impl LogLevelHandle {
    pub fn new(handle: FilterHandle, initial: String) -> Self {
        Self { handle, current: Arc::new(Mutex::new(initial)) }
    }

    /// The active filter string.
    pub fn current(&self) -> String {
        self.current.lock().map(|g| g.clone()).unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Replace the filter.
    ///
    /// The string is parsed before it is installed, so an invalid directive is
    /// a rejected request rather than a daemon that has silently stopped
    /// logging — which would be the worst possible outcome for a debugging
    /// feature.
    ///
    /// Length is bounded first. `EnvFilter` accepts a bare word as a target
    /// name, so an arbitrarily long string parses successfully and is then held
    /// resident for the life of the process — an unauthenticated caller growing
    /// the daemon's footprint against a 32 MiB ceiling, one request at a time.
    /// Found by probing this endpoint during the M01 DAST pass, which accepted
    /// a 2 MB filter and returned 200.
    pub fn set(&self, filter: &str) -> Result<(), String> {
        if filter.len() > MAX_FILTER_LEN {
            return Err(format!(
                "filter is {} bytes, over the {MAX_FILTER_LEN}-byte limit",
                filter.len()
            ));
        }

        let parsed = EnvFilter::try_new(filter).map_err(|e| e.to_string())?;

        self.handle.reload(parsed).map_err(|e| e.to_string())?;

        // Poisoning only means some other thread panicked while holding this;
        // the string itself is still valid, so recover rather than propagate.
        match self.current.lock() {
            Ok(mut g) => *g = filter.to_string(),
            Err(e) => *e.into_inner() = filter.to_string(),
        }

        tracing::info!(filter, "log level changed");
        Ok(())
    }
}

impl std::fmt::Debug for LogLevelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogLevelHandle").field("current", &self.current()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set` is only reachable with a live subscriber, so the length check is
    /// tested as the pure predicate it is. The alternative — installing a global
    /// subscriber from a test — would make the whole test binary order-dependent.
    #[test]
    fn the_length_bound_is_generous_for_real_filters() {
        for filter in [
            "info",
            "debug",
            "dafs_scan=trace,dafs_store=debug,info",
            "dafs_api::logging=trace,dafs_scan::watch=debug,warn",
        ] {
            assert!(
                filter.len() <= MAX_FILTER_LEN,
                "a realistic filter {filter:?} would be rejected by the {MAX_FILTER_LEN}-byte bound"
            );
        }
    }

    /// The case the DAST pass found: a bare word parses as a target name, so an
    /// arbitrarily long string is a valid filter and would be held resident.
    #[test]
    fn an_oversized_filter_is_over_the_bound() {
        let huge = "a".repeat(2 * 1024 * 1024);
        assert!(EnvFilter::try_new(&huge).is_ok(), "premise: a bare word parses as a filter");
        assert!(huge.len() > MAX_FILTER_LEN, "so the length bound is what has to reject it");
    }
}
