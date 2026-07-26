//! Integration test for `dafs self-update`'s embedded script.
//!
//! `main.rs` embeds `scripts/install.sh` via `include_str!` and execs it
//! through a real `sh` — this asserts the artifact that actually ships is
//! valid, and that the flags `self_update_script_args` builds are ones the
//! script actually understands. Both run against the real files on disk and
//! a real `sh` subprocess, not an in-process fake.

use std::path::PathBuf;
use std::process::Command;

fn install_script_path() -> PathBuf {
    // Same relative path main.rs's `include_str!` uses, from the crate root
    // rather than from src/, so a future reader can find both by inspection.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

#[test]
fn the_embedded_script_is_syntactically_valid_posix_sh() {
    let path = install_script_path();
    assert!(path.is_file(), "expected {} to exist", path.display());

    let status = Command::new("sh")
        .arg("-n") // parse only, don't execute
        .arg(&path)
        .status()
        .expect("spawning sh -n");

    assert!(status.success(), "scripts/install.sh has a syntax error");
}

/// The flags `self_update_script_args` (in `src/main.rs`) constructs must be
/// ones the script's own argument parser actually recognizes — otherwise
/// `dafs self-update` would exec a script that immediately rejects its own
/// invocation.
#[test]
fn the_script_recognizes_every_flag_self_update_can_pass() {
    let contents = std::fs::read_to_string(install_script_path()).expect("reading install.sh");

    for flag in ["--self-update", "--check-only", "--target-path", "--current-version"] {
        assert!(contents.contains(flag), "install.sh's arg parser has no case for {flag}");
    }
}
