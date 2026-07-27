//! Registers the [`sqlite-vec`](https://github.com/asg017/sqlite-vec) SQLite
//! extension so `dafs-store` can create and query `vec0` virtual tables.
//!
//! # Why this is its own crate
//!
//! `dafs-store` (`forbid(unsafe_code)`) and every other crate that touches a
//! SQLite connection stay unsafe-free. Exactly one call in this whole
//! workspace needs to be `unsafe` — `rusqlite::ffi::sqlite3_auto_extension`,
//! which registers a C function pointer to run against every connection
//! opened afterwards — and this crate exists to be the one place that holds
//! it, the same reason `dafs-alloc` exists to hold jemalloc's `malloc_conf`
//! registration. `deny(unsafe_code)` with one scoped `#[allow]`, not `forbid`,
//! for the same reason `dafs-alloc` uses `deny`: `forbid` cannot be
//! downgraded per-item, and this crate has exactly one item that needs to be.
//!
//! # Why vendored as source, not a runtime-loaded extension
//!
//! `docs/roadmap-and-design-review.md` §2 item 9's vendoring principle (first
//! applied to pdfium: commit the artifact, keep `cargo build` hermetic and
//! network-free) applies here without needing pdfium's `include_bytes!`
//! workaround at all — the upstream `sqlite-vec` crate vendors
//! `sqlite-vec.c` (a single ~10k-line amalgamation) and compiles it with
//! `cc` at build time, linking it directly into the binary. There is no
//! shared library to extract to a cache file on first run, no
//! `load_extension` call, and no runtime dependency on where SQLite looks
//! for extensions — `cargo build --offline` already covers it the same way
//! it covers rusqlite's own bundled SQLite.
//!
//! # Why registration is global, not per-connection
//!
//! `sqlite3_auto_extension` installs a hook that runs against *every*
//! SQLite connection opened by this process from the moment it's called
//! onward, including ones opened by other crates or test harnesses that know
//! nothing about vectors. That is the only registration mechanism this
//! extension has — there is no per-`Connection::open` alternative — so
//! [`register`] must run before the first `Connection::open` anywhere in the
//! process, not once per call site. `dafs-store::open`/`open_in_memory` call
//! it internally for exactly this reason: every connection this workspace
//! opens goes through those two functions, so callers never need to know
//! this crate exists.

#![deny(unsafe_code)]

use std::sync::Once;

static REGISTER_ONCE: Once = Once::new();

/// Registers the `vec0` module against every SQLite connection this process
/// opens from now on. Idempotent — safe to call from every code path that
/// might be the first to open a connection, without coordinating who goes
/// first.
///
/// # Soundness
///
/// Identical to the call `sqlite-vec`'s own crate tests against itself
/// (`sqlite-vec-0.1.9/src/lib.rs`, `test_rusqlite_auto_extension`): take the
/// address of `sqlite3_vec_init` as a raw pointer and transmute it to the
/// function-pointer type `sqlite3_auto_extension` expects. The Rust-side
/// declaration of `sqlite3_vec_init` (`extern "C" fn()`, no arguments) is
/// deliberately not its real C signature — SQLite's extension entry-point
/// convention takes three arguments — but it is never called as a Rust `fn`
/// on this side of the FFI boundary, only passed by address to C code that
/// calls it with the real signature. That is the one transmute this crate
/// exists to hold.
pub fn register() {
    REGISTER_ONCE.call_once(|| {
        #[allow(unsafe_code)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn vec0_is_usable_after_register() {
        register();
        let conn = Connection::open_in_memory().expect("open");
        let version: String =
            conn.query_row("SELECT vec_version()", [], |r| r.get(0)).expect("vec_version()");
        assert!(!version.is_empty());
    }

    #[test]
    fn register_is_idempotent() {
        // Calling register() twice (as two independent test-suite processes
        // effectively do, and as a daemon restarted in-process during a test
        // harness would) must not panic or double-register the module.
        register();
        register();
        let conn = Connection::open_in_memory().expect("open");
        let version: String =
            conn.query_row("SELECT vec_version()", [], |r| r.get(0)).expect("vec_version()");
        assert!(!version.is_empty());
    }

    #[test]
    fn a_vec0_table_can_be_created_and_queried() {
        register();
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE VIRTUAL TABLE v USING vec0(embedding float[3]);")
            .expect("create vec0 table");

        let a: Vec<u8> = [1.0f32, 0.0, 0.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        let b: Vec<u8> = [0.0f32, 1.0, 0.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        conn.execute("INSERT INTO v(rowid, embedding) VALUES (1, ?1)", [a]).expect("insert a");
        conn.execute("INSERT INTO v(rowid, embedding) VALUES (2, ?1)", [b]).expect("insert b");

        let query: Vec<u8> = [1.0f32, 0.0, 0.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        let nearest: i64 = conn
            .query_row(
                "SELECT rowid FROM v WHERE embedding MATCH ?1 ORDER BY distance LIMIT 1",
                [query],
                |r| r.get(0),
            )
            .expect("knn query");
        assert_eq!(nearest, 1, "the identical vector should be its own nearest neighbour");
    }
}
