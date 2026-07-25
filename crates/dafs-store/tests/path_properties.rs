//! Property tests for path interning and reconstruction.
//!
//! These assert the same invariants as `fuzz/fuzz_targets/paths.rs`, over a
//! fixed set of adversarial inputs rather than a fuzzer's random ones. Both
//! exist deliberately:
//!
//! - The fuzz target explores inputs nobody thought of, but only runs where a
//!   nightly toolchain and `cargo-fuzz` are available, and its findings are not
//!   permanent.
//! - This runs on every `cargo test`, on every platform, forever. Each case
//!   here is a filename that is *legal on a real filesystem* and would be
//!   plausible to get wrong.
//!
//! Where a fuzzer finds a crash, its input belongs in the table below — that is
//! the cumulative-regression discipline the testing bar asks for, applied to
//! paths.

use std::path::{Path, PathBuf};

use dafs_store::paths::{Interner, ensure_dir_chain, resolve_path};

/// Filenames that are legal on a Unix filesystem and easy to mishandle.
fn adversarial_names() -> Vec<&'static str> {
    vec![
        "ordinary.txt",
        // Spaces and shell metacharacters: legal, and a reminder that nothing
        // here may ever be interpolated into a shell command.
        "file with spaces.txt",
        "quote'and\"double.txt",
        "semi;colon && pipe|.txt",
        "$(command substitution).txt",
        "`backticks`.txt",
        // SQL-shaped, because the store is SQLite and these must be values
        // rather than syntax.
        "'; DROP TABLE files; --",
        "100% sure.txt",
        "under_score%wildcard.txt",
        // Traversal-shaped as a *literal name*, which is legal and distinct
        // from a real `..` component.
        "..hidden",
        "...",
        "a..b",
        // Unicode, including characters that are visually confusable or change
        // text direction.
        "café.txt",
        "日本語.txt",
        "emoji-📁.txt",
        "\u{202e}gnp.txt",
        // Combining characters: two different byte sequences that render
        // identically. They must intern as distinct components, because the
        // filesystem treats them as distinct files.
        "e\u{0301}.txt",
        "é.txt",
        // Whitespace that is easy to trim by accident.
        " leading.txt",
        "trailing.txt ",
        "tab\there.txt",
        "newline\nhere.txt",
        // Long, but under the 255-byte per-component limit.
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.txt",
    ]
}

/// Mirror of the store's normalisation.
///
/// A separate implementation on purpose: calling the store's own would make the
/// round-trip assertion hold by construction and test nothing.
fn expected_path(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return "/".into();
    }
    parts.iter().fold(String::new(), |mut acc, p| {
        acc.push('/');
        acc.push_str(p);
        acc
    })
}

#[test]
fn adversarial_filenames_round_trip() {
    let conn = dafs_store::open_in_memory().expect("open");
    let mut interner = Interner::new();

    for name in adversarial_names() {
        let path = Path::new("/home/user").join(name);

        let id = ensure_dir_chain(&conn, &mut interner, &path)
            .unwrap_or_else(|e| panic!("interning {name:?} failed: {e}"));

        let resolved = resolve_path(&conn, id).expect("resolve");
        assert_eq!(resolved, expected_path(&path), "path did not round-trip for name {name:?}");
    }
}

/// Names that render identically but differ in bytes are different files, and
/// must not be merged into one row.
#[test]
fn visually_identical_names_stay_distinct() {
    let conn = dafs_store::open_in_memory().expect("open");
    let mut interner = Interner::new();

    // "é" precomposed vs "e" + combining acute.
    let precomposed = ensure_dir_chain(&conn, &mut interner, Path::new("/x/\u{e9}.txt")).unwrap();
    let decomposed = ensure_dir_chain(&conn, &mut interner, Path::new("/x/e\u{301}.txt")).unwrap();

    assert_ne!(
        precomposed, decomposed,
        "two byte-distinct filenames were merged into one file row"
    );
}

/// Interning is a function: one name, one id, regardless of cache state or how
/// many times it is seen.
#[test]
fn interning_is_deterministic_across_cache_states() {
    let conn = dafs_store::open_in_memory().expect("open");

    let mut warm = Interner::new();
    let mut ids = Vec::new();
    for name in adversarial_names() {
        ids.push(ensure_dir_chain(&conn, &mut warm, &Path::new("/home/user").join(name)).unwrap());
    }

    // Same paths, a cache that has never seen them, and reverse order — so an
    // id that depended on insertion order or cache residency would differ.
    let mut cold = Interner::new();
    for (name, expected) in adversarial_names().iter().zip(&ids).rev() {
        let again =
            ensure_dir_chain(&conn, &mut cold, &Path::new("/home/user").join(name)).unwrap();
        assert_eq!(again, *expected, "re-interning {name:?} yielded a different id");
    }
}

/// A deep path must not blow the stack or truncate silently.
///
/// `resolve_path` bounds its walk at 256 components; this stays under that so
/// the assertion is about correctness rather than the bound.
#[test]
fn deep_paths_round_trip() {
    let conn = dafs_store::open_in_memory().expect("open");
    let mut interner = Interner::new();

    let mut path = PathBuf::from("/");
    for depth in 0..200 {
        path.push(format!("d{depth}"));
    }

    let id = ensure_dir_chain(&conn, &mut interner, &path).expect("deep chain");
    assert_eq!(resolve_path(&conn, id).expect("resolve"), expected_path(&path));
}

/// A path deeper than the resolver's bound must truncate rather than hang or
/// panic — the daemon staying responsive matters more than a complete answer
/// for a path no real filesystem produces.
#[test]
fn paths_past_the_resolver_bound_truncate_safely() {
    let conn = dafs_store::open_in_memory().expect("open");
    let mut interner = Interner::new();

    let mut path = PathBuf::from("/");
    for depth in 0..400 {
        path.push(format!("d{depth}"));
    }

    let id = ensure_dir_chain(&conn, &mut interner, &path).expect("very deep chain");
    let resolved = resolve_path(&conn, id).expect("resolve must not fail");

    assert!(!resolved.is_empty(), "resolution returned nothing at all");
    assert!(
        resolved.len() < path.to_string_lossy().len(),
        "a path past the bound should be truncated"
    );
}

/// Non-UTF-8 filenames are legal on Unix and must be handled rather than
/// rejected — a file the scanner cannot name is a file missing from the
/// timeline with no explanation.
#[test]
#[cfg(unix)]
fn non_utf8_filenames_are_interned() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let conn = dafs_store::open_in_memory().expect("open");
    let mut interner = Interner::new();

    // 0xFF is never valid UTF-8.
    let name = OsString::from_vec(vec![b'b', b'a', b'd', 0xFF, b'.', b't', b'x', b't']);
    let path = Path::new("/home/user").join(&name);

    let id = ensure_dir_chain(&conn, &mut interner, &path).expect("non-utf8 name");
    let resolved = resolve_path(&conn, id).expect("resolve");

    // Lossy conversion means the stored name carries U+FFFD rather than the
    // original byte. That is a deliberate trade — see `paths::normalised_components`
    // — and it is asserted here so the behaviour is a decision on the record
    // rather than a surprise.
    assert!(resolved.contains('\u{FFFD}'), "expected a replacement char, got {resolved:?}");
    assert!(resolved.starts_with("/home/user/bad"), "unexpected path {resolved:?}");
}
