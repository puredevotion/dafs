//! The DAFS daemon.
//!
//! M00: open and migrate the metadata store, serve the HTTP API, shut down
//! cleanly on a signal. No file watching, no indexing, no AI — those are M01+.
//!
//! The startup order matters and is asserted by the integration tests: bind the
//! listener *before* migrating, so `/healthz` answers during a long migration,
//! but only `set_ready()` afterwards so `/readyz` stays 503 until the schema is
//! actually usable.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;

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
    #[arg(long, env = "DAFS_LOG", default_value = "info")]
    log: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_tracing(&args.log)?;

    // Single-threaded until there is concurrent work to justify otherwise. A
    // multi-thread runtime pre-spawns a worker per core, each with its own
    // stack, which is measurable against a 32 MB idle ceiling for no benefit
    // while the daemon only serves a handful of API requests.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(run(args))
}

async fn run(args: Args) -> anyhow::Result<()> {
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

    // Held rather than used: M01 hands this to the file-watcher task. Dropping
    // it here would close the database and discard the WAL tuning.
    let _conn = conn;

    let state = dafs_api::AppState::new(schema_version);
    state.set_ready();
    tracing::info!(schema_version, "ready");

    axum::serve(listener, dafs_api::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving HTTP API")?;

    tracing::info!("shut down cleanly");
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

fn init_tracing(filter: &str) -> anyhow::Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};

    let env_filter =
        EnvFilter::try_new(filter).with_context(|| format!("invalid log filter {filter:?}"))?;

    fmt().with_env_filter(env_filter).with_target(true).try_init().map_err(|e| {
        // try_init fails only if a subscriber is already set, which in a binary
        // means main ran twice — worth surfacing rather than ignoring.
        anyhow::anyhow!("installing tracing subscriber: {e}")
    })
}
