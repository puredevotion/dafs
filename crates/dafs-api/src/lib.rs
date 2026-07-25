//! HTTP surface.
//!
//! M00 shipped the router and the health/readiness/version endpoints. M01 adds
//! `/events` (the timeline) and `/log-level`. Search arrives at M03 as another
//! route on this router.
//!
//! # Why health and readiness are separate
//!
//! `/healthz` answers "is this process alive", `/readyz` answers "can it serve
//! requests". They differ for this daemon: the process starts and binds before
//! the metadata database has necessarily finished migrating, and during a long
//! migration it is alive but not ready. Collapsing them would make a deployment
//! either kill a daemon mid-migration (if the probe is liveness) or route
//! traffic at an unmigrated database (if it is readiness).

#![forbid(unsafe_code)]

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};

mod logging;
pub mod timeline;

pub use logging::LogLevelHandle;
pub use timeline::{TimelineItem, TimelineReader, TimelineStats, TimelineStore};

/// Shared daemon state handed to every route.
///
/// The store arrives as a [`TimelineReader`] trait object rather than a
/// connection: rusqlite's `Connection` is not `Sync`, and keeping the concrete
/// type out of this crate means the HTTP layer can be tested without a database
/// and does not depend on a storage engine. The daemon supplies the
/// implementation — see `dafs_api::timeline` for the connection-shape decision
/// M00 deferred to here.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<StateInner>,
}

struct StateInner {
    /// Schema version the store reached at startup. Surfaced by `/version` so a
    /// deployment can tell which schema a running daemon actually has, rather
    /// than inferring it from the binary's version.
    schema_version: u32,
    /// Flipped once startup work (migrations) has completed.
    ready: std::sync::atomic::AtomicBool,
    started_at: std::time::Instant,
    /// Set once the daemon wires in a store. `None` in unit tests of routes
    /// that do not touch it, and in M00-era callers.
    timeline: Option<TimelineReader>,
    /// Set when the daemon installs a reloadable tracing filter.
    log_level: Option<LogLevelHandle>,
}

impl AppState {
    pub fn new(schema_version: u32) -> Self {
        Self {
            inner: Arc::new(StateInner {
                schema_version,
                ready: std::sync::atomic::AtomicBool::new(false),
                started_at: std::time::Instant::now(),
                timeline: None,
                log_level: None,
            }),
        }
    }

    /// Attach the timeline store. Builder-style because `AppState` is shared
    /// behind an `Arc` and must be fully built before it is cloned into routes.
    pub fn with_timeline(mut self, reader: TimelineReader) -> Self {
        Self::inner_mut(&mut self).timeline = Some(reader);
        self
    }

    /// Attach the runtime log-level handle.
    pub fn with_log_level(mut self, handle: LogLevelHandle) -> Self {
        Self::inner_mut(&mut self).log_level = Some(handle);
        self
    }

    /// Mutable access during construction.
    ///
    /// Sound because the builders above run before any clone exists, so the
    /// `Arc` is unique. It panics rather than silently ignoring the call if that
    /// ever stops being true — a builder that quietly did nothing would be a
    /// miserable bug to find.
    fn inner_mut(state: &mut Self) -> &mut StateInner {
        Arc::get_mut(&mut state.inner)
            .expect("AppState builders must run before the state is shared")
    }

    pub fn timeline(&self) -> Option<&TimelineReader> {
        self.inner.timeline.as_ref()
    }

    pub fn log_level(&self) -> Option<&LogLevelHandle> {
        self.inner.log_level.as_ref()
    }

    /// Mark the daemon ready to serve. Called once startup work finishes.
    pub fn set_ready(&self) {
        self.inner.ready.store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn schema_version(&self) -> u32 {
        self.inner.schema_version
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.inner.started_at.elapsed()
    }
}

/// The timeline UI, embedded in the binary.
///
/// This is `ui/dist/index.html` — the Vite build output, a single file with its
/// JavaScript and CSS inlined. Embedding rather than serving from disk keeps the
/// daemon one deployable artifact with no runtime asset path to get wrong.
///
/// `ui/dist/` is **committed**, and that is a deliberate trade. The Rust build
/// must work with no network (CI vendors crates and builds `--offline`), so an
/// `npm ci` cannot sit in front of `cargo build`. Committing the bundle keeps
/// the Rust side hermetic and the Nix flake free of node entirely. CI rebuilds
/// the frontend and fails if the committed copy differs, so the source and the
/// artifact cannot drift — see `.github/workflows/ci.yml`.
const UI_INDEX: &str = include_str!("../../../ui/dist/index.html");

/// Build the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui_index))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .route("/metrics", get(metrics))
        .route("/events", get(events))
        // GET reads the level, PUT changes it. See `logging` for why a write
        // endpoint is acceptable on an unauthenticated API bound to loopback,
        // and what has to change if that bind ever widens.
        .route("/log-level", get(get_log_level).put(set_log_level))
        .fallback(not_found)
        .with_state(state)
}

/// Query parameters for `/events`.
#[derive(Debug, Deserialize)]
struct EventsQuery {
    limit: Option<u32>,
    before_id: Option<i64>,
    kind: Option<String>,
}

/// The timeline.
async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> impl IntoResponse {
    let Some(reader) = state.timeline().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError { error: "timeline store not attached" }),
        )
            .into_response();
    };

    // Validate before touching the store: an unknown `kind` is a client error,
    // and letting it through would silently return an empty page that looks
    // like "you have no events" rather than "you asked for something that does
    // not exist".
    if let Some(kind) = &query.kind
        && !matches!(kind.as_str(), "created" | "modified" | "deleted" | "renamed")
    {
        return (StatusCode::BAD_REQUEST, Json(ApiError { error: "unknown event kind" }))
            .into_response();
    }

    // spawn_blocking because the store takes a mutex around a synchronous
    // SQLite call. Awaiting that inline would stall the single-threaded runtime
    // — including the health probes — for the length of the query.
    let result = tokio::task::spawn_blocking(move || {
        reader.timeline(
            query.limit.unwrap_or(DEFAULT_EVENT_LIMIT),
            query.before_id,
            query.kind.as_deref(),
        )
    })
    .await;

    match result {
        Ok(Ok(items)) => {
            // The cursor for the next page, so a client does not have to know
            // that pagination keys on id rather than timestamp.
            let next_before_id = items.last().map(|i| i.id);
            (StatusCode::OK, Json(EventsResponse { next_before_id, events: items })).into_response()
        }
        Ok(Err(e)) => {
            tracing::error!("timeline query failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "timeline query failed" }))
                .into_response()
        }
        Err(e) => {
            // The blocking task panicked. Log it and fail this request rather
            // than letting the panic escape and take the daemon down.
            tracing::error!("timeline task panicked: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "timeline task failed" }))
                .into_response()
        }
    }
}

/// Default page size when the caller does not ask for one. The store clamps the
/// upper bound; this is only the default.
const DEFAULT_EVENT_LIMIT: u32 = 50;

#[derive(Serialize)]
struct EventsResponse {
    events: Vec<timeline::TimelineItem>,
    /// Pass as `before_id` to fetch the next page. Absent when the page was
    /// empty, which is how a client knows it has reached the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_before_id: Option<i64>,
}

#[derive(Serialize)]
struct LogLevel {
    filter: String,
}

async fn get_log_level(State(state): State<AppState>) -> impl IntoResponse {
    match state.log_level() {
        Some(handle) => {
            (StatusCode::OK, Json(LogLevel { filter: handle.current() })).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError { error: "log level control not attached" }),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct SetLogLevel {
    /// A tracing `EnvFilter` directive, e.g. `info`, `debug`,
    /// `dafs_scan=trace,info`.
    filter: String,
}

async fn set_log_level(
    State(state): State<AppState>,
    Json(body): Json<SetLogLevel>,
) -> impl IntoResponse {
    let Some(handle) = state.log_level() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError { error: "log level control not attached" }),
        )
            .into_response();
    };

    match handle.set(&body.filter) {
        Ok(()) => (StatusCode::OK, Json(LogLevel { filter: handle.current() })).into_response(),
        // A bad directive is the caller's error, and rejecting it leaves the
        // previous filter in force — a daemon that silently stopped logging
        // would be the worst outcome for a debugging feature.
        Err(e) => {
            tracing::warn!("rejected log filter {:?}: {e}", body.filter);
            (StatusCode::BAD_REQUEST, Json(ApiError { error: "invalid log filter" }))
                .into_response()
        }
    }
}

async fn ui_index() -> impl IntoResponse {
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], UI_INDEX)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

/// Liveness. Answers as long as the process can serve a request at all.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(Health { status: "ok" }))
}

#[derive(Serialize)]
struct Readiness {
    ready: bool,
    schema_version: u32,
}

/// Readiness. 503 until startup work has finished, so a deployment does not
/// route traffic at a daemon whose database is still migrating.
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let ready = state.is_ready();
    let body = Json(Readiness { ready, schema_version: state.schema_version() });
    let code = if ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, body)
}

#[derive(Serialize)]
struct Version {
    version: &'static str,
    schema_version: u32,
    uptime_seconds: u64,
}

async fn version(State(state): State<AppState>) -> impl IntoResponse {
    Json(Version {
        version: env!("CARGO_PKG_VERSION"),
        schema_version: state.schema_version(),
        uptime_seconds: state.uptime().as_secs(),
    })
}

/// Minimal Prometheus-format metrics.
///
/// RSS is exported from M00 deliberately: the memory budget is a hard
/// requirement, so it needs to be observable in a running deployment and not
/// only asserted in CI. A ceiling that is only ever checked by the test suite
/// gets discovered to be wrong in production.
async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let mut out = String::with_capacity(512);

    out.push_str("# HELP dafs_ready Whether the daemon has finished startup work.\n");
    out.push_str("# TYPE dafs_ready gauge\n");
    out.push_str(&format!("dafs_ready {}\n", u8::from(state.is_ready())));

    out.push_str("# HELP dafs_schema_version Applied metadata schema version.\n");
    out.push_str("# TYPE dafs_schema_version gauge\n");
    out.push_str(&format!("dafs_schema_version {}\n", state.schema_version()));

    out.push_str("# HELP dafs_uptime_seconds Seconds since process start.\n");
    out.push_str("# TYPE dafs_uptime_seconds counter\n");
    out.push_str(&format!("dafs_uptime_seconds {}\n", state.uptime().as_secs()));

    if let Some(rss) = dafs_alloc::resident_bytes() {
        out.push_str("# HELP dafs_resident_bytes Process resident set size.\n");
        out.push_str("# TYPE dafs_resident_bytes gauge\n");
        out.push_str(&format!("dafs_resident_bytes {rss}\n"));
    }

    // Store counts, so a deployment can see the observer is actually observing.
    // A daemon that is healthy, ready, and recording nothing looks identical to
    // a working one on every other metric here.
    //
    // Queried inline rather than through spawn_blocking: these are two indexed
    // counts, and /metrics is scraped on a timer by one client. If they ever
    // become slow enough to matter, they should be cached rather than made
    // async, since a scrape blocking on the store is a bad shape either way.
    if let Some(reader) = state.timeline() {
        match reader.stats() {
            Ok(stats) => {
                out.push_str("# HELP dafs_events_total Events recorded in the store.\n");
                out.push_str("# TYPE dafs_events_total counter\n");
                out.push_str(&format!("dafs_events_total {}\n", stats.events));

                out.push_str("# HELP dafs_files_known Files currently known, excluding deleted.\n");
                out.push_str("# TYPE dafs_files_known gauge\n");
                out.push_str(&format!("dafs_files_known {}\n", stats.files));
            }
            Err(e) => {
                // A failed stat must not fail the scrape: the RSS and readiness
                // gauges above are exactly what someone needs when the store is
                // unhappy.
                tracing::warn!("timeline stats unavailable for /metrics: {e}");
            }
        }
    }

    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")], out)
}

#[derive(Serialize)]
struct ApiError {
    error: &'static str,
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Json(ApiError { error: "not found" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for oneshot

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn healthz_is_ok_before_ready() {
        // Liveness must not depend on readiness — this is the distinction the
        // module docs describe, and the test that stops them being merged.
        let state = AppState::new(1);
        let app = router(state);
        let resp = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_503_until_ready() {
        let state = AppState::new(1);
        let app = router(state.clone());
        let resp = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        state.set_ready();
        let app = router(state);
        let resp = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn version_reports_schema_version() {
        let app = router(AppState::new(42));
        let resp = app
            .oneshot(Request::builder().uri("/version").body(Body::empty()).unwrap())
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("\"schema_version\":42"), "unexpected body: {body}");
    }

    #[tokio::test]
    async fn metrics_exposes_rss_and_parses_as_prometheus() {
        let state = AppState::new(1);
        state.set_ready();
        let app = router(state);
        let resp = app
            .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;

        assert!(body.contains("dafs_ready 1"), "ready gauge missing: {body}");
        assert!(body.contains("dafs_schema_version 1"), "schema gauge missing");

        // Every non-comment line must be `name value` with a numeric value —
        // a malformed exposition breaks scrapers silently.
        for line in body.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            let (name, value) = line.split_once(' ').unwrap_or_else(|| {
                panic!("metric line has no value: {line:?}");
            });
            assert!(!name.is_empty(), "empty metric name in {line:?}");
            value.parse::<f64>().unwrap_or_else(|_| {
                panic!("metric value not numeric: {line:?}");
            });
        }

        #[cfg(target_os = "linux")]
        assert!(body.contains("dafs_resident_bytes"), "RSS metric missing on linux");
    }

    #[tokio::test]
    async fn ui_shell_is_served_at_root() {
        let app = router(AppState::new(1));
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("<title>DAFS</title>"), "UI not served");

        // The bundle must be self-contained: the daemon serves exactly this one
        // string and has no route for sibling assets, so a build that emitted a
        // separate .js or .css would render a blank page in production while
        // every Rust test still passed.
        assert!(
            !body.contains("<script type=\"module\" src=\"/"),
            "UI references an external script; the single-file build has regressed"
        );
        assert!(
            !body.contains("rel=\"stylesheet\" href=\"/"),
            "UI references an external stylesheet; the single-file build has regressed"
        );

        // The endpoints the page depends on. If a route is renamed, this fails
        // rather than the UI silently going blank against a live daemon.
        assert!(body.contains("/events"), "UI should read the timeline");
        assert!(body.contains("/version"), "UI should poll /version");
        assert!(body.contains("dafs_resident_bytes"), "UI should read the RSS metric");
    }

    /// Build a state with a fake store holding `n` events, newest id highest.
    fn state_with_events(n: i64) -> AppState {
        let items = (1..=n)
            .rev()
            .map(|id| timeline::TimelineItem {
                id,
                path: format!("/home/u/file-{id}.txt"),
                kind: if id == 1 { "created" } else { "modified" }.to_string(),
                at_unix_ms: 1_000 + id,
                size_bytes: Some(id * 10),
                is_dir: false,
                previous_path: None,
            })
            .collect();

        AppState::new(1)
            .with_timeline(Arc::new(timeline::testing::FakeStore { items, fail: false }))
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, String) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .expect("request");
        let status = resp.status();
        (status, body_string(resp).await)
    }

    #[tokio::test]
    async fn events_returns_the_timeline() {
        let (status, body) = get(router(state_with_events(3)), "/events").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("file-3.txt"), "unexpected body: {body}");
        assert!(body.contains("\"next_before_id\""), "no pagination cursor: {body}");
    }

    #[tokio::test]
    async fn events_without_a_store_is_503_not_an_empty_list() {
        // An empty list would tell a client "you have no history", which is a
        // different and much worse claim than "this daemon cannot answer yet".
        let (status, _) = get(router(AppState::new(1)), "/events").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_unknown_event_kind_is_rejected() {
        let (status, body) = get(router(state_with_events(3)), "/events?kind=exploded").await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "an unknown kind should be rejected, not silently return nothing: {body}"
        );
    }

    #[tokio::test]
    async fn a_known_event_kind_is_accepted() {
        let (status, _) = get(router(state_with_events(3)), "/events?kind=created").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_store_failure_is_a_500_not_a_panic() {
        let state = AppState::new(1)
            .with_timeline(Arc::new(timeline::testing::FakeStore { items: vec![], fail: true }));

        let (status, _) = get(router(state), "/events").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn metrics_reports_store_counts_when_a_store_is_attached() {
        let (_, body) = get(router(state_with_events(7)), "/metrics").await;

        assert!(body.contains("dafs_events_total 7"), "event count missing: {body}");
        assert!(body.contains("dafs_files_known"), "file gauge missing: {body}");
    }

    /// A failing store must not break the scrape — RSS and readiness are exactly
    /// what a reader needs when the store is unhappy.
    #[tokio::test]
    async fn metrics_still_serves_when_the_store_fails() {
        let state = AppState::new(1)
            .with_timeline(Arc::new(timeline::testing::FakeStore { items: vec![], fail: true }));

        let (status, body) = get(router(state), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("dafs_ready"), "core metrics lost: {body}");
        assert!(!body.contains("dafs_events_total"), "a failed stat was reported anyway");
    }

    #[tokio::test]
    async fn log_level_without_a_handle_is_503() {
        let (status, _) = get(router(AppState::new(1)), "/log-level").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn unknown_route_is_json_404() {
        let app = router(AppState::new(1));
        let resp = app
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(body_string(resp).await.contains("not found"));
    }
}
