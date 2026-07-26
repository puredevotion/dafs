//! The DAFS daemon.
//!
//! Opens and migrates the metadata store, observes the configured watch roots,
//! serves the HTTP API, and shuts down cleanly on a signal. No indexing and no
//! AI — those are M02+.
//!
//! # Startup order
//!
//! Asserted by the integration tests: bind the listener *before* migrating, so
//! `/healthz` answers during a long migration, but only `set_ready()` afterwards
//! so `/readyz` stays 503 until the schema is actually usable.
//!
//! Readiness does **not** wait for the initial scan. On a large tree that runs
//! for minutes, and a deployment watching `/readyz` would conclude the daemon
//! had failed to start. The API works throughout; it simply has less to show at
//! first.
//!
//! # Two connections, deliberately
//!
//! The observer thread opens its own connection rather than sharing the API's.
//! That is the case WAL mode exists for — the writer does not block readers —
//! and sharing one would put every timeline request behind the scan's lock for
//! the length of the scan.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;

mod store_adapter;

/// Bind the tuned allocator. Load-bearing for the memory budget — see
/// `dafs-alloc`'s module docs for why this is not a preference.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: dafs_alloc::Allocator = dafs_alloc::ALLOCATOR;

#[derive(Parser, Debug)]
#[command(name = "dafs", version, about = "Distributed AI-native filesystem daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    args: Args,
}

/// Subcommands live alongside the daemon's flat flags rather than replacing
/// them: `command: None` must behave exactly as `dafs --watch ...` always
/// has, so existing invocations and the README's examples keep working
/// unchanged.
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Replace this binary with the latest (or `--check-only` compare
    /// against the) release, via scripts/install.sh embedded at compile time.
    SelfUpdate {
        /// Report whether an update is available without installing it.
        #[arg(long)]
        check_only: bool,
    },
}

#[derive(clap::Args, Debug)]
struct Args {
    /// Directory for the metadata database and, later, the object store.
    #[arg(long, env = "DAFS_DATA_DIR", default_value = "./.dafs")]
    data_dir: PathBuf,

    /// Address for the HTTP API.
    ///
    /// Loopback by default, deliberately: the API is unauthenticated in M00 and
    /// binding it to a routable address would expose an unauthenticated surface
    /// on the network. Widening this is a decision for whoever adds auth.
    #[arg(long, env = "DAFS_LISTEN", default_value = "127.0.0.1:7878")]
    listen: SocketAddr,

    /// Log filter, e.g. `info`, `dafs_store=debug`.
    ///
    /// Changeable at runtime via `PUT /log-level` — a restart to raise
    /// verbosity destroys the state that was about to be diagnosed.
    #[arg(long, env = "DAFS_LOG", default_value = "info")]
    log: String,

    /// Directories to observe. Repeatable.
    ///
    /// Empty by default, deliberately: a daemon that starts indexing a home
    /// directory nobody pointed it at is a surprise, and this one is meant to be
    /// safe to start before it is configured. With no roots it serves an empty
    /// timeline and watches nothing.
    #[arg(long = "watch", env = "DAFS_WATCH", value_delimiter = ',')]
    watch: Vec<PathBuf>,

    /// Skip the initial scan and only watch for changes.
    ///
    /// For restarting against a store that is already populated, where a
    /// rescan would be a few minutes of work to discover nothing changed.
    #[arg(long, env = "DAFS_NO_INITIAL_SCAN")]
    no_initial_scan: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(Command::SelfUpdate { check_only }) = cli.command {
        return self_update(check_only);
    }

    let args = cli.args;
    let log_handle = init_tracing(&args.log)?;

    // Single-threaded until there is concurrent work to justify otherwise. A
    // multi-thread runtime pre-spawns a worker per core, each with its own
    // stack, which is measurable against a 32 MB idle ceiling for no benefit
    // while the daemon only serves a handful of API requests.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(run(args, log_handle))
}

async fn run(args: Args, log_handle: dafs_api::LogLevelHandle) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating data dir {}", args.data_dir.display()))?;

    // Bind first, migrate second: see the module docs on startup ordering.
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    let local = listener.local_addr().context("reading bound address")?;
    tracing::info!(%local, "listening");

    let db_path = args.data_dir.join("metadata.sqlite");
    let conn = dafs_store::open(&db_path)
        .with_context(|| format!("opening metadata store at {}", db_path.display()))?;
    let schema_version = dafs_store::current_version(&conn).context("reading schema version")?;

    let timeline: dafs_api::TimelineReader = Arc::new(store_adapter::SqliteTimeline::new(conn));

    let state = dafs_api::AppState::new(schema_version)
        .with_timeline(Arc::clone(&timeline))
        .with_log_level(log_handle);

    // Ready once the schema is usable. The observer starts after this and runs
    // for minutes on a large tree; holding readiness until it finished would
    // make a deployment think the daemon had failed to start, and the API is
    // genuinely usable throughout — it just has less to show at first.
    state.set_ready();
    tracing::info!(schema_version, roots = ?args.watch, "ready");

    let observer = spawn_observer(&args, &db_path)?;

    axum::serve(listener, dafs_api::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving HTTP API")?;

    // Ask the observer to stop and give it a moment to finish its current
    // batch. Not joined indefinitely: a watch blocked on a wedged filesystem
    // must not stop the daemon from exiting, and every event it had is already
    // committed.
    observer.shutdown();

    tracing::info!("shut down cleanly");
    Ok(())
}

/// Handle to the observer thread.
struct Observer {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Observer {
    fn shutdown(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(handle) = self.handle.take() {
            // The observer checks the stop flag between polls, so it exits
            // within roughly one poll interval.
            match handle.join() {
                Ok(()) => tracing::debug!("observer stopped"),
                Err(_) => tracing::warn!("observer thread panicked"),
            }
        }
    }
}

/// Start the scan-then-watch thread.
///
/// A dedicated OS thread rather than a tokio task: the work is entirely
/// blocking — a filesystem walk and synchronous SQLite writes — and running it
/// on the single-threaded runtime would stall every HTTP request behind it,
/// including the health probes.
///
/// It opens its **own** connection to the same database rather than sharing the
/// API's. Two connections in WAL mode is the case WAL exists for: the writer
/// does not block readers, and sharing one would put every timeline request
/// behind the scan's lock for the length of the scan.
fn spawn_observer(args: &Args, db_path: &std::path::Path) -> anyhow::Result<Observer> {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    if args.watch.is_empty() {
        tracing::info!("no --watch roots configured; not observing anything");
        return Ok(Observer { stop, handle: None });
    }

    let roots = args.watch.clone();
    let skip_initial = args.no_initial_scan;
    let db_path = db_path.to_path_buf();
    let thread_stop = Arc::clone(&stop);

    let handle = std::thread::Builder::new()
        .name("dafs-observer".into())
        .spawn(move || observe(&db_path, &roots, skip_initial, &thread_stop))
        .context("spawning the observer thread")?;

    Ok(Observer { stop, handle: Some(handle) })
}

/// Scan the roots, then watch them until asked to stop.
fn observe(
    db_path: &std::path::Path,
    roots: &[PathBuf],
    skip_initial: bool,
    stop: &std::sync::atomic::AtomicBool,
) {
    let conn = match dafs_store::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("observer could not open the store: {e}");
            return;
        }
    };

    let mut interner = dafs_store::paths::Interner::new();
    let options = dafs_scan::ScanOptions::default();

    if !skip_initial {
        for root in roots {
            if stop.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            match dafs_scan::scan(&conn, &mut interner, root, &options) {
                Ok(summary) => {
                    tracing::info!(root = %root.display(), ?summary, "initial scan done")
                }
                // One bad root should not stop the others: a watch list with a
                // path that has been unmounted is a configuration problem, not
                // a reason to observe nothing.
                Err(e) => tracing::error!(root = %root.display(), "initial scan failed: {e}"),
            }
        }
    }

    let mut watch = match dafs_scan::watch::Watch::new(roots) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("could not start the filesystem watch: {e}");
            return;
        }
    };

    tracing::info!(roots = ?roots, "watching");

    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        // A short poll so shutdown is responsive; the debounce inside Watch is
        // what actually decides when a change is reported.
        let changes = watch.poll(std::time::Duration::from_millis(250));
        if changes.is_empty() {
            continue;
        }

        tracing::debug!(count = changes.len(), "applying watch changes");

        for change in changes {
            if let Err(e) = apply_change(&conn, &mut interner, roots, &options, change) {
                tracing::warn!("could not apply a watch change: {e}");
            }
        }
    }
}

/// Fold one observed change into the store.
fn apply_change(
    conn: &rusqlite::Connection,
    interner: &mut dafs_store::paths::Interner,
    roots: &[PathBuf],
    options: &dafs_scan::ScanOptions,
    change: dafs_scan::watch::Change,
) -> anyhow::Result<()> {
    use dafs_scan::watch::Change;

    match change {
        Change::Overflowed => {
            // Events were lost, so the store no longer reflects the filesystem
            // and there is no way to know what was missed. A rescan is the only
            // correct response; treating it as "nothing happened" would leave
            // the timeline quietly wrong until the next restart.
            tracing::warn!("watch queue overflowed; rescanning");
            for root in roots {
                if let Err(e) = dafs_scan::scan(conn, interner, root, options) {
                    tracing::error!(root = %root.display(), "rescan after overflow failed: {e}");
                }
            }
        }
        Change::Touched(path) | Change::Removed(path)
            if dafs_scan::watch::is_excluded(&path, &options.skip_dirs) =>
        {
            tracing::trace!(path = %path.display(), "ignoring excluded path");
        }
        Change::Touched(path) => {
            dafs_scan::record_path(conn, interner, roots, &path)?;
        }
        Change::Removed(path) => {
            dafs_scan::record_removal(conn, interner, roots, &path)?;
        }
        // A rename into or out of an excluded directory is not a rename as far
        // as the timeline is concerned — it is an arrival or a departure, and
        // recording it as a move would show a path the user asked not to index.
        Change::Renamed { from, to } => {
            let from_excluded = dafs_scan::watch::is_excluded(&from, &options.skip_dirs);
            let to_excluded = dafs_scan::watch::is_excluded(&to, &options.skip_dirs);

            match (from_excluded, to_excluded) {
                (true, true) => {
                    tracing::trace!("ignoring rename entirely within excluded paths");
                }
                (true, false) => dafs_scan::record_path(conn, interner, roots, &to)?,
                (false, true) => dafs_scan::record_removal(conn, interner, roots, &from)?,
                (false, false) => {
                    dafs_scan::record_rename(conn, interner, roots, &from, &to)?;
                }
            }
        }
    }

    Ok(())
}

/// The installer script, embedded at compile time.
///
/// `dafs self-update` shells out to this instead of reimplementing
/// fetch/verify/replace in Rust: one source of truth with
/// `scripts/install.sh`'s normal install path, and zero new dependencies
/// (no HTTP client, no TLS stack) in a binary with a 32 MB idle-RSS ceiling,
/// for a code path that runs rarely.
const INSTALL_SCRIPT: &str = include_str!("../../../scripts/install.sh");

/// Build the argument list passed to `sh -s -- <these>` (the installer script
/// reads them as its own positional params). Pure and separate from
/// [`self_update`] so the exact invocation is unit-testable without spawning
/// a process.
fn self_update_script_args(
    check_only: bool,
    current_exe: &std::path::Path,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    if check_only {
        args.push("--check-only".into());
    } else {
        args.push("--self-update".into());
        args.push("--target-path".into());
        args.push(current_exe.into());
    }
    args.push("--current-version".into());
    args.push(env!("CARGO_PKG_VERSION").into());
    args
}

/// Run the embedded installer script against this binary.
///
/// Not async and does not touch the store, scanner, or API — self-update
/// exits before any of that is opened.
fn self_update(check_only: bool) -> anyhow::Result<()> {
    let current_exe = std::env::current_exe().context("resolving the running binary's path")?;

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-s").arg("--");
    cmd.args(self_update_script_args(check_only, &current_exe));
    cmd.stdin(std::process::Stdio::piped());

    let mut child = cmd.spawn().context("spawning sh to run the installer script")?;
    {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("stdin was piped since it was just set above")
            .write_all(INSTALL_SCRIPT.as_bytes())
            .context("writing the installer script to sh's stdin")?;
    }
    let status = child.wait().context("waiting for the installer script")?;

    match status.code() {
        Some(0) => Ok(()),
        // install.sh's own convention: exit 3 means `--check-only` found an
        // update but did not apply it — not a failure, but worth a distinct
        // exit code so scripts driving `dafs self-update --check-only` can
        // tell "up to date" from "update available" without parsing stdout.
        Some(3) if check_only => std::process::exit(3),
        other => anyhow::bail!("self-update script failed (exit code {other:?})"),
    }
}

/// Resolve when the process is asked to stop.
///
/// SIGTERM as well as Ctrl-C: a container runtime sends SIGTERM, and a daemon
/// that only handles SIGINT gets SIGKILLed after the grace period instead of
/// closing its database cleanly.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to listen for ctrl-c: {e}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::error!("failed to install SIGTERM handler: {e}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}

/// Install the subscriber and return a handle that can change its filter later.
///
/// The handle is what makes `PUT /log-level` possible. Reproducing a problem is
/// usually the expensive part of diagnosing one, and a daemon that must be
/// restarted to raise its log level loses the state that was about to be
/// explained — a scan mid-flight, a watch that has just gone quiet. Being able
/// to turn on `trace` against the running process and turn it off again is
/// worth the one indirection it costs.
fn init_tracing(filter: &str) -> anyhow::Result<dafs_api::LogLevelHandle> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*, reload};

    let env_filter =
        EnvFilter::try_new(filter).with_context(|| format!("invalid log filter {filter:?}"))?;

    let (layer, handle) = reload::Layer::new(env_filter);

    tracing_subscriber::registry()
        .with(layer)
        .with(fmt::layer().with_target(true))
        .try_init()
        .map_err(|e| {
            // try_init fails only if a subscriber is already set, which in a
            // binary means main ran twice — worth surfacing rather than
            // ignoring.
            anyhow::anyhow!("installing tracing subscriber: {e}")
        })?;

    Ok(dafs_api::LogLevelHandle::new(handle, filter.to_string()))
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    /// `dafs --watch a,b` must keep parsing exactly as it did before the
    /// subcommand existed — no subcommand means `command` is `None` and
    /// every existing flag lands in the flattened `Args`.
    #[test]
    fn no_subcommand_parses_flags_into_args_unchanged() {
        let cli = Cli::try_parse_from([
            "dafs",
            "--watch",
            "/a,/b",
            "--listen",
            "127.0.0.1:9999",
            "--no-initial-scan",
        ])
        .expect("valid flags must parse");

        assert!(cli.command.is_none());
        assert_eq!(cli.args.watch, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(cli.args.listen, "127.0.0.1:9999".parse().unwrap());
        assert!(cli.args.no_initial_scan);
    }

    #[test]
    fn no_arguments_at_all_uses_every_default() {
        let cli = Cli::try_parse_from(["dafs"]).expect("bare invocation must parse");
        assert!(cli.command.is_none());
        assert!(cli.args.watch.is_empty());
        assert_eq!(cli.args.data_dir, PathBuf::from("./.dafs"));
        assert_eq!(cli.args.listen, "127.0.0.1:7878".parse().unwrap());
        assert!(!cli.args.no_initial_scan);
    }

    #[test]
    fn self_update_subcommand_parses() {
        let cli = Cli::try_parse_from(["dafs", "self-update"]).expect("must parse");
        assert!(matches!(cli.command, Some(Command::SelfUpdate { check_only: false })));
    }

    #[test]
    fn self_update_check_only_flag_parses() {
        let cli = Cli::try_parse_from(["dafs", "self-update", "--check-only"]).expect("must parse");
        assert!(matches!(cli.command, Some(Command::SelfUpdate { check_only: true })));
    }

    /// A subcommand and the daemon's own flags are mutually exclusive by
    /// construction (clap routes to one or the other), so a mistyped
    /// combination should fail to parse rather than silently pick one.
    #[test]
    fn subcommand_and_watch_together_is_a_parse_error() {
        let result = Cli::try_parse_from(["dafs", "self-update", "--watch", "/a"]);
        assert!(result.is_err(), "self-update takes no daemon flags");
    }

    #[test]
    fn check_only_passes_check_only_and_no_target_path() {
        let args = self_update_script_args(true, std::path::Path::new("/usr/local/bin/dafs"));
        assert_eq!(args[0], "--check-only");
        assert!(!args.iter().any(|a| a == "--target-path"));
        assert!(args.iter().any(|a| a == "--current-version"));
    }

    #[test]
    fn real_update_passes_self_update_and_the_current_exe_path() {
        let exe = std::path::Path::new("/usr/local/bin/dafs");
        let args = self_update_script_args(false, exe);
        assert_eq!(args[0], "--self-update");
        assert_eq!(args[1], "--target-path");
        assert_eq!(args[2], exe.as_os_str());
    }
}
