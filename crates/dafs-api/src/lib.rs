//! HTTP surface.
//!
//! M00 ships the router, the health/readiness/version endpoints, and the state
//! plumbing — no timeline or search yet. Those arrive in M01/M03 as new routes
//! on this router.
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

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::Serialize;

/// Shared daemon state handed to every route.
///
/// The SQLite connection is deliberately *not* in here yet. rusqlite's
/// `Connection` is not `Sync`, and the right shape (a blocking thread owning the
/// connection, or a small pool) depends on the query mix that M01 introduces.
/// Committing to a wrong shape now would be harder to undo than adding it later.
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
}

impl AppState {
    pub fn new(schema_version: u32) -> Self {
        Self {
            inner: Arc::new(StateInner {
                schema_version,
                ready: std::sync::atomic::AtomicBool::new(false),
                started_at: std::time::Instant::now(),
            }),
        }
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

/// The M00 UI shell, embedded in the binary.
///
/// Embedded rather than read from disk so the daemon is a single deployable
/// artifact with no runtime asset path to get wrong. It is ~2 KiB; when M01
/// brings a real frontend this becomes a decision about a static-file service,
/// but embedding one small file now costs nothing and keeps deployment to
/// "copy one binary".
const UI_INDEX: &str = include_str!("../../../ui/index.html");

/// Build the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui_index))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .route("/metrics", get(metrics))
        .fallback(not_found)
        .with_state(state)
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
        assert!(body.contains("<title>DAFS</title>"), "UI shell not served");
        // The shell reads /version and /metrics; if those route names change,
        // this catches the shell going stale rather than silently breaking.
        assert!(body.contains("/version"), "shell should poll /version");
        assert!(body.contains("dafs_resident_bytes"), "shell should read the RSS metric");
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
