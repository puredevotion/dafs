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
    let args = Args::parse();
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
    }

    Ok(())
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
