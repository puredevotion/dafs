//! Double-fork daemonization for `dafs --detach`.
//!
//! `dafs-daemon` forbids unsafe code in its own source; the fork/setsid dance
//! genuinely needs some (see below), so it lives here instead, kept as small
//! and as reviewable as possible.
//!
//! # The sequence
//!
//! 1. Fork. The parent exits immediately — this is what returns control to
//!    the invoking shell right away.
//! 2. The child calls `setsid()`, becoming a new session leader with no
//!    controlling terminal.
//! 3. Fork again. The (session-leader) parent exits. Only a session leader
//!    can acquire a controlling terminal, and the second fork gives that up
//!    permanently — the grandchild can never reacquire one even if it opens
//!    a tty device later.
//! 4. The grandchild `chdir`s to `data_dir` and redirects stdin to
//!    `/dev/null`, stdout and stderr to `log_path`. This is the process that
//!    returns from `start()` and continues as the actual daemon.

#![deny(unsafe_code)]

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use nix::unistd::ForkResult;

fn to_io_error(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

/// Fork the calling process into the background.
///
/// # Safety invariant this relies on
///
/// `fork()` is `unsafe` because forking a multi-threaded process is only
/// sound if the child touches nothing that depended on state another thread
/// might have held a lock on at the moment of the fork (allocator internals,
/// in particular). This is sound here because `start` must be called before
/// any thread but the main one exists — before the tokio runtime is built
/// and before the observer thread is spawned — which `dafs-daemon`'s `main`
/// guarantees by calling it first.
#[allow(unsafe_code)]
fn fork_and_exit_parent() -> io::Result<()> {
    // Safety: see the function doc above — single-threaded at every call site.
    match unsafe { nix::unistd::fork() }.map_err(to_io_error)? {
        ForkResult::Parent { .. } => std::process::exit(0),
        ForkResult::Child => Ok(()),
    }
}

/// Fork into the background, detach from the controlling terminal, and
/// redirect stdin/stdout/stderr. Only the final grandchild process returns
/// from this call.
pub fn start(data_dir: &Path, log_path: &Path) -> io::Result<()> {
    fork_and_exit_parent()?;
    nix::unistd::setsid().map_err(to_io_error)?;
    fork_and_exit_parent()?;

    nix::unistd::chdir(data_dir).map_err(to_io_error)?;
    redirect_stdio(log_path)?;

    Ok(())
}

fn redirect_stdio(log_path: &Path) -> io::Result<()> {
    let devnull = std::fs::OpenOptions::new().read(true).open("/dev/null")?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_path)?;

    nix::unistd::dup2(devnull.as_raw_fd(), 0).map_err(to_io_error)?;
    nix::unistd::dup2(log.as_raw_fd(), 1).map_err(to_io_error)?;
    nix::unistd::dup2(log.as_raw_fd(), 2).map_err(to_io_error)?;

    Ok(())
}
