//! Library half of `dafs-tui`, split out from the binary so `tests/` can
//! exercise [`client::Client`] against a real HTTP server rather than only
//! the pure formatting helpers unit-tested in place.

#![forbid(unsafe_code)]

pub mod capabilities;
pub mod client;
pub mod ui;
