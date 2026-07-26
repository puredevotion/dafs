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

use std::io::IsTerminal as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use clap::Parser;

mod control_client;
mod detach;
mod pidfile;
mod store_adapter;
mod watch_adapter;

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

    /// Stop a running daemon for this data directory.
    ///
    /// Its own `--data-dir` rather than sharing `Args`'s: a subcommand and the
    /// daemon's own flags are mutually exclusive by clap's own construction
    /// (see the parsing tests), so this needs the one flag it actually uses.
    Stop {
        #[arg(long, env = "DAFS_DATA_DIR", default_value = "./.dafs")]
        data_dir: PathBuf,
    },
}

/// What to do when `--watch` roots are given but a daemon for this data-dir
/// is already running: extend its watch list, replace it outright, or leave
/// it alone. `Cancel` exists so a non-interactive caller can explicitly
/// no-op rather than the absence of a flag meaning that by default.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[clap(rename_all = "lowercase")]
enum OnRunning {
    Add,
    Replace,
    Cancel,
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

    /// Fork into the background and log to `<data-dir>/dafs.log` instead of
    /// the terminal. Without this, a daemon started with `&` keeps writing
    /// to whatever else shares that terminal — including, in practice, a
    /// `dafs-tui` in the same window, whose alternate-screen rendering that
    /// corrupts.
    #[arg(long, env = "DAFS_DETACH")]
    detach: bool,

    /// When `--watch` roots are given but a daemon is already running for
    /// this data-dir: add to its watch list, replace it, or cancel. Required
    /// when stdin isn't a terminal to prompt on; optional otherwise.
    #[arg(long, value_enum, env = "DAFS_ON_RUNNING")]
    on_running: Option<OnRunning>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(Command::SelfUpdate { check_only }) = cli.command {
        return self_update(check_only);
    }
    if let Some(Command::Stop { data_dir }) = cli.command {
        return stop_daemon(&data_dir);
    }

    let args = cli.args;

    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating data dir {}", args.data_dir.display()))?;
    let canonical_data_dir = args
        .data_dir
        .canonicalize()
        .with_context(|| format!("resolving data dir {}", args.data_dir.display()))?;

    // Checked here, before any daemonization and before a tokio runtime
    // exists: whether a live daemon already owns this data-dir decides
    // whether this process is going to bind anything at all, and forking
    // only makes sense once that's settled.
    if let Some(existing) = pidfile::find_live(&canonical_data_dir) {
        return reconcile_with_running_daemon(&args, existing);
    }

    if args.detach {
        detach::start(&canonical_data_dir)?;
    }

    let log_handle = init_tracing(&args.log)?;

    // Single-threaded until there is concurrent work to justify otherwise. A
    // multi-thread runtime pre-spawns a worker per core, each with its own
    // stack, which is measurable against a 32 MB idle ceiling for no benefit
    // while the daemon only serves a handful of API requests.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(run(args, log_handle, canonical_data_dir))
}

async fn run(
    args: Args,
    log_handle: dafs_api::LogLevelHandle,
    canonical_data_dir: PathBuf,
) -> anyhow::Result<()> {
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

    let shared_roots = Arc::new(Mutex::new(args.watch.clone()));
    let (command_tx, command_rx) = mpsc::channel();
    let watch_control: dafs_api::WatchControlHandle =
        Arc::new(watch_adapter::DaemonWatchControl::new(Arc::clone(&shared_roots), command_tx));

    let state = dafs_api::AppState::new(schema_version)
        .with_timeline(Arc::clone(&timeline))
        .with_log_level(log_handle)
        .with_watch_control(watch_control);

    // Ready once the schema is usable. The observer starts after this and runs
    // for minutes on a large tree; holding readiness until it finished would
    // make a deployment think the daemon had failed to start, and the API is
    // genuinely usable throughout — it just has less to show at first.
    state.set_ready();
    tracing::info!(schema_version, roots = ?args.watch, "ready");

    let observer = spawn_observer(&args, &db_path, shared_roots, command_rx)?;

    // The requested IP (so a wildcard bind address, which a reconciling
    // client could not usefully connect back to, never ends up in the
    // pidfile) combined with the actually-bound port (so `--listen ...:0`,
    // used in tests to avoid port collisions, records the real port rather
    // than the literal `0` that was requested).
    let pidfile_addr = SocketAddr::new(args.listen.ip(), local.port());
    pidfile::write(&canonical_data_dir, pidfile_addr).context("writing pidfile")?;

    axum::serve(listener, dafs_api::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving HTTP API")?;

    // Ask the observer to stop and give it a moment to finish its current
    // batch. Not joined indefinitely: a watch blocked on a wedged filesystem
    // must not stop the daemon from exiting, and every event it had is already
    // committed.
    observer.shutdown();
    pidfile::remove(&canonical_data_dir);

    tracing::info!("shut down cleanly");
    Ok(())
}

/// Reached when `--watch` (or nothing) is given but a live daemon already
/// owns this data-dir. Fully synchronous — no tokio runtime exists yet, and
/// none is needed for a couple of small blocking HTTP calls to a sibling
/// process on loopback.
fn reconcile_with_running_daemon(args: &Args, existing: pidfile::PidFile) -> anyhow::Result<()> {
    let addr = existing.listen;

    if args.watch.is_empty() {
        let roots =
            control_client::get_roots(addr).context("querying the running daemon's watch roots")?;
        println!("dafs already running (pid {}) at {addr}, watching:", existing.pid);
        print_roots(&roots);
        return Ok(());
    }

    let new_roots: Vec<String> = args.watch.iter().map(|p| p.display().to_string()).collect();
    let current_roots =
        control_client::get_roots(addr).context("querying the running daemon's watch roots")?;

    let mode = match args.on_running {
        Some(mode) => mode,
        None if std::io::stdin().is_terminal() => {
            prompt_add_or_replace(&current_roots, &new_roots)?
        }
        None => anyhow::bail!(
            "dafs is already running (pid {}), watching {current_roots:?}. Pass \
             --on-running=add, --on-running=replace, or --on-running=cancel to \
             reconfigure it non-interactively.",
            existing.pid
        ),
    };

    let mode_str = match mode {
        OnRunning::Add => "add",
        OnRunning::Replace => "replace",
        OnRunning::Cancel => {
            println!("left the running daemon (pid {}) unchanged.", existing.pid);
            return Ok(());
        }
    };

    let updated = control_client::change_roots(addr, mode_str, new_roots)
        .context("reconfiguring the running daemon")?;

    println!("dafs (pid {}) now watching:", existing.pid);
    print_roots(&updated);
    Ok(())
}

fn print_roots(roots: &[String]) {
    if roots.is_empty() {
        println!("  (nothing yet)");
    }
    for root in roots {
        println!("  {root}");
    }
}

/// Interactive fallback when `--on-running` wasn't given and stdin is a
/// terminal. Defaults to cancelling on an unrecognized answer — a
/// misunderstood prompt should never quietly replace a running daemon's
/// watch list.
fn prompt_add_or_replace(current: &[String], new: &[String]) -> anyhow::Result<OnRunning> {
    use std::io::Write as _;

    println!("dafs is already running, watching:");
    print_roots(current);
    println!("you asked to watch:");
    print_roots(new);
    print!("[a]dd as more roots, [r]eplace the current roots, [c]ancel? ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("reading your answer")?;
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "a" | "add" => OnRunning::Add,
        "r" | "replace" => OnRunning::Replace,
        _ => OnRunning::Cancel,
    })
}

/// `dafs stop`: signal a running daemon and wait for it to actually exit.
fn stop_daemon(data_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("resolving data dir {}", data_dir.display()))?;
    let canonical = data_dir
        .canonicalize()
        .with_context(|| format!("resolving data dir {}", data_dir.display()))?;

    let Some(existing) = pidfile::find_live(&canonical) else {
        println!("no running dafs daemon found for {}", canonical.display());
        return Ok(());
    };

    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(existing.pid.to_string())
        .status()
        .context("running kill")?;
    if !status.success() {
        anyhow::bail!("kill -TERM {} failed (exit code {:?})", existing.pid, status.code());
    }

    // Graceful shutdown flushes the store and removes the pidfile itself —
    // a caller scripting around `dafs stop` needs that to have actually
    // happened, not just that the signal was sent.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if pidfile::find_live(&canonical).is_none() {
            println!("stopped dafs (pid {})", existing.pid);
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    anyhow::bail!("dafs (pid {}) did not stop within 10s", existing.pid)
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
fn spawn_observer(
    args: &Args,
    db_path: &std::path::Path,
    shared_roots: Arc<Mutex<Vec<PathBuf>>>,
    command_rx: mpsc::Receiver<watch_adapter::WatchCommand>,
) -> anyhow::Result<Observer> {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Spawned unconditionally, even with zero initial roots: a live
    // `PUT /watch` needs a thread to receive the command, and a daemon
    // started with nothing to watch yet is exactly the case that command
    // exists for.
    let initial_roots = args.watch.clone();
    let skip_initial = args.no_initial_scan;
    let db_path = db_path.to_path_buf();
    let thread_stop = Arc::clone(&stop);

    let handle = std::thread::Builder::new()
        .name("dafs-observer".into())
        .spawn(move || {
            observe(&db_path, initial_roots, skip_initial, &thread_stop, shared_roots, command_rx)
        })
        .context("spawning the observer thread")?;

    Ok(Observer { stop, handle: Some(handle) })
}

/// Scan the initial roots, then watch them — and whatever live
/// add/replace commands bring in — until asked to stop.
fn observe(
    db_path: &std::path::Path,
    initial_roots: Vec<PathBuf>,
    skip_initial: bool,
    stop: &std::sync::atomic::AtomicBool,
    shared_roots: Arc<Mutex<Vec<PathBuf>>>,
    command_rx: mpsc::Receiver<watch_adapter::WatchCommand>,
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
        for root in &initial_roots {
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

    let mut watch = match dafs_scan::watch::Watch::new(&initial_roots) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("could not start the filesystem watch: {e}");
            return;
        }
    };

    // The live root list. `initial_roots` alone no longer suffices once a
    // live add/replace can change it — this is what rescans-on-overflow and
    // rename-exclusion checks use from here on.
    let mut roots = initial_roots;
    if roots.is_empty() {
        tracing::info!("no watch roots configured yet; idle until one is added live");
    } else {
        tracing::info!(?roots, "watching");
    }

    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        // Applied before polling for filesystem changes: a root just added
        // needs its initial scan done before the watch on it means anything.
        while let Ok(cmd) = command_rx.try_recv() {
            apply_watch_command(&conn, &mut interner, &mut watch, &mut roots, &options, cmd);
            if let Ok(mut shared) = shared_roots.lock() {
                *shared = roots.clone();
            }
        }

        // A short poll so shutdown is responsive; the debounce inside Watch is
        // what actually decides when a change is reported.
        let changes = watch.poll(std::time::Duration::from_millis(250));
        if changes.is_empty() {
            continue;
        }

        tracing::debug!(count = changes.len(), "applying watch changes");

        for change in changes {
            if let Err(e) = apply_change(&conn, &mut interner, &roots, &options, change) {
                tracing::warn!("could not apply a watch change: {e}");
            }
        }
    }
}

/// Apply one live reconfiguration command: scan (and start watching) any new
/// root before touching the old ones, so a `replace` never leaves a gap
/// where nothing is being watched at all.
fn apply_watch_command(
    conn: &rusqlite::Connection,
    interner: &mut dafs_store::paths::Interner,
    watch: &mut dafs_scan::watch::Watch,
    roots: &mut Vec<PathBuf>,
    options: &dafs_scan::ScanOptions,
    cmd: watch_adapter::WatchCommand,
) {
    use watch_adapter::WatchCommand;

    /// Scans and starts watching one root, folding any failure into `errors`
    /// rather than stopping — one bad root among several should not silence
    /// the daemon's reply about the ones that worked.
    fn add_one(
        conn: &rusqlite::Connection,
        interner: &mut dafs_store::paths::Interner,
        watch: &mut dafs_scan::watch::Watch,
        roots: &mut Vec<PathBuf>,
        options: &dafs_scan::ScanOptions,
        root: PathBuf,
        errors: &mut Vec<String>,
    ) {
        if roots.contains(&root) {
            tracing::info!(root = %root.display(), "already watched, ignoring");
            return;
        }
        match dafs_scan::scan(conn, interner, &root, options) {
            Ok(summary) => {
                tracing::info!(root = %root.display(), ?summary, "added root, initial scan done")
            }
            Err(e) => {
                let msg = format!("scan of {} failed: {e}", root.display());
                tracing::error!("{msg}");
                errors.push(msg);
                return;
            }
        }
        if let Err(e) = watch.add_root(&root) {
            let msg = format!("failed to watch {}: {e}", root.display());
            tracing::error!("{msg}");
            errors.push(msg);
            return;
        }
        roots.push(root);
    }

    let (new_roots, replacing, reply) = match cmd {
        WatchCommand::AddRoots { roots: new_roots, reply } => (new_roots, false, reply),
        WatchCommand::ReplaceRoots { roots: new_roots, reply } => (new_roots, true, reply),
    };

    if replacing {
        for old in roots.drain(..) {
            if let Err(e) = watch.remove_root(&old) {
                tracing::warn!(root = %old.display(), "failed to unwatch old root: {e}");
            }
        }
    }

    let mut errors = Vec::new();
    for root in new_roots {
        add_one(conn, interner, watch, roots, options, root, &mut errors);
    }

    let result = if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) };
    // The caller may have given up waiting (timeout) — nothing to do but
    // drop the result, which `send` already does on a disconnected channel.
    let _ = reply.send(result);
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

    #[test]
    fn stop_subcommand_parses_with_its_own_data_dir() {
        let cli = Cli::try_parse_from(["dafs", "stop", "--data-dir", "/x"]).expect("must parse");
        let Some(Command::Stop { data_dir }) = cli.command else {
            panic!("expected Command::Stop, got {:?}", cli.command);
        };
        assert_eq!(data_dir, Path::new("/x"));
    }

    #[test]
    fn stop_defaults_its_data_dir_like_the_daemon_does() {
        let cli = Cli::try_parse_from(["dafs", "stop"]).expect("must parse");
        let Some(Command::Stop { data_dir }) = cli.command else {
            panic!("expected Command::Stop, got {:?}", cli.command);
        };
        assert_eq!(data_dir, Path::new("./.dafs"));
    }

    #[test]
    fn detach_and_on_running_default_off() {
        let cli = Cli::try_parse_from(["dafs"]).expect("must parse");
        assert!(!cli.args.detach);
        assert!(cli.args.on_running.is_none());
    }

    #[test]
    fn detach_and_on_running_parse() {
        let cli = Cli::try_parse_from(["dafs", "--detach", "--on-running", "replace"])
            .expect("must parse");
        assert!(cli.args.detach);
        assert_eq!(cli.args.on_running, Some(OnRunning::Replace));
    }
}
