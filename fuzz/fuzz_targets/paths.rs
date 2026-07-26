//! Fuzz path interning and reconstruction.
//!
//! # Why this target exists
//!
//! Filenames are bytes the user did not type — they arrive from whatever is on
//! disk, including archives, sync clients, and other machines. On Unix a
//! filename is any byte sequence except NUL and `/`, which means invalid UTF-8,
//! embedded newlines, and names that look like path traversal are all *legal*
//! and all reachable by simply scanning a directory. The testing bar requires a
//! fuzz target for every parser touching bytes the user did not type, and this
//! is one.
//!
//! # The properties asserted
//!
//! 1. **No panic, no hang.** Any byte sequence must be interned or rejected,
//!    never crash. `resolve_path`'s bounded walk is part of this: a cycle must
//!    terminate rather than spin inside what will later be an HTTP handler.
//! 2. **Round-trip stability.** A path that goes in comes back out as the same
//!    string. If interning and reconstruction disagree, the timeline shows a
//!    file at a location it is not, which for a tool whose entire purpose is
//!    telling you what happened to your files is the worst kind of wrong.
//! 3. **Interning is a function.** The same component always yields the same id.
//!    Two ids for one name would silently split a file's history in two.
#![no_main]

use std::path::PathBuf;

use dafs_store::paths::{Interner, ensure_dir_chain, resolve_path};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Split the input into path components on NUL, which is the one byte a
    // filename cannot contain — so this cannot accidentally produce a component
    // the filesystem could not have handed us.
    let components: Vec<Vec<u8>> =
        data.split(|b| *b == 0).filter(|c| !c.is_empty()).take(64).map(<[u8]>::to_vec).collect();

    if components.is_empty() {
        return;
    }

    let Ok(conn) = dafs_store::open_in_memory() else {
        return;
    };
    let mut interner = Interner::new();

    // Build a path from the fuzzed components. `from_vec` keeps non-UTF-8 bytes
    // intact rather than replacing them, which is the case worth testing —
    // going through String would sanitise away the interesting inputs.
    #[cfg(unix)]
    let path: PathBuf = {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let mut path = PathBuf::from("/");
        for component in &components {
            path.push(OsString::from_vec(component.clone()));
        }
        path
    };

    #[cfg(not(unix))]
    let path: PathBuf = {
        let mut path = PathBuf::from("/");
        for component in &components {
            path.push(String::from_utf8_lossy(component).as_ref());
        }
        path
    };

    // Property 1: never panics. A component that normalises away (`.`, `..`
    // past the root) legitimately yields EmptyPath, which is an error, not a
    // crash.
    let Ok(id) = ensure_dir_chain(&conn, &mut interner, &path) else {
        return;
    };

    // Property 2: what was stored is what comes back.
    let Ok(resolved) = resolve_path(&conn, id) else {
        return;
    };

    // The comparison is against the *normalised* path, not the raw input:
    // `ensure_dir_chain` resolves `.` and `..` textually, so a path containing
    // them is expected to come back shorter. Re-normalising the same way is
    // what makes this an assertion about round-tripping rather than about
    // normalisation.
    let expected = normalise(&path);
    assert_eq!(
        resolved, expected,
        "path did not round-trip: stored {path:?}, got back {resolved:?}, expected {expected:?}"
    );

    // Property 3: interning is deterministic, including across a cold cache.
    let mut cold = Interner::new();
    if let Ok(again) = ensure_dir_chain(&conn, &mut cold, &path) {
        assert_eq!(again, id, "re-interning {path:?} yielded a different file id");
    }
});

/// Mirror of the crate's internal normalisation, for the round-trip assertion.
///
/// Deliberately a separate implementation rather than a call into the store: if
/// both sides used the same function, the assertion would hold by construction
/// and test nothing.
fn normalise(path: &std::path::Path) -> String {
    let mut parts: Vec<String> = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => {
                parts.push(part.to_string_lossy().into_owned());
            }
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }

    if parts.is_empty() {
        return "/".to_string();
    }

    let mut out = String::new();
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    out
}
