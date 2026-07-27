mod countdown;
mod html_convert;
mod mcp_server;
mod metrics;
mod resume;
mod resume_route;
mod state;
mod wordpress;

use std::sync::Arc;

use axum::Router;
use axum::extract::{MatchedPath, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

use crate::mcp_server::InfoServer;
use crate::state::{AppError, AppState};

const DEFAULT_WORDPRESS_URL: &str = "https://cooperlees.com";
const DEFAULT_RESUME_DOC_ID: &str = "1ksWGBa1ZrGVItQybR-tVnism2E-LIGsmcdf-CDalFcw";
const DEFAULT_COUNTDOWN_URL: &str = "https://countdown.cooperlees.com";
const DEFAULT_LISTEN_PORT: u16 = 6969;
/// rmcp's Streamable HTTP transport rejects requests whose `Host` header
/// isn't in this list (DNS-rebinding protection) — it defaults to loopback
/// only, so a public deployment behind Traefik must add its own hostname via
/// `ALLOWED_HOSTS`, or every real request gets a 403.
const DEFAULT_ALLOWED_HOSTS: &str = "localhost,127.0.0.1,::1";

struct Config {
    wordpress_url: String,
    resume_doc_id: String,
    countdown_url: String,
    listen_port: u16,
    allowed_hosts: Vec<String>,
}

impl Config {
    fn from_env() -> Result<Self, AppError> {
        let wordpress_url =
            std::env::var("WORDPRESS_URL").unwrap_or_else(|_| DEFAULT_WORDPRESS_URL.to_owned());
        let resume_doc_id =
            std::env::var("RESUME_DOC_ID").unwrap_or_else(|_| DEFAULT_RESUME_DOC_ID.to_owned());
        let countdown_url =
            std::env::var("COUNTDOWN_URL").unwrap_or_else(|_| DEFAULT_COUNTDOWN_URL.to_owned());
        let listen_port = match std::env::var("LISTEN_PORT") {
            Ok(raw) => raw.parse::<u16>().map_err(|e| {
                AppError::Other(format!("LISTEN_PORT {raw:?} is not a valid port: {e}"))
            })?,
            Err(_) => DEFAULT_LISTEN_PORT,
        };
        let allowed_hosts = std::env::var("ALLOWED_HOSTS")
            .unwrap_or_else(|_| DEFAULT_ALLOWED_HOSTS.to_owned())
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            wordpress_url,
            resume_doc_id,
            countdown_url,
            listen_port,
            allowed_hosts,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::from_env()?;
    let state = AppState::new(
        config.wordpress_url.clone(),
        config.resume_doc_id.clone(),
        config.countdown_url.clone(),
    )?;

    let mcp_state = state.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(InfoServer::new(mcp_state.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_allowed_hosts(config.allowed_hosts.clone()),
    );

    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .route("/", get(root))
        .route("/coopers-resume", get(resume_route::coopers_resume))
        .route("/resume", get(resume_route::coopers_resume))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            track_http_metrics,
        ))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.listen_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| AppError::Other(format!("failed to bind {addr}: {e}")))?;

    tracing::info!(%addr, "mcp_info_server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| AppError::Other(format!("server error: {e}")))?;

    Ok(())
}

/// Records `mcp_info_server_http_requests_total` and
/// `_http_request_duration_seconds` for every request, labeled by the
/// matched route template (never the raw path — `/mcp` carries every MCP
/// tool call, so per-tool breakdowns come from `mcp_server.rs`'s own
/// instrumentation instead of by route here).
async fn track_http_metrics(
    State(state): State<AppState>,
    matched_path: Option<MatchedPath>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let method = req.method().to_string();
    let route = matched_path
        .as_ref()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_owned();

    let timer = state
        .metrics
        .http_request_duration_seconds
        .with_label_values(&[&route, &method])
        .start_timer();
    let response = next.run(req).await;
    timer.observe_duration();

    let status = response.status().as_u16().to_string();
    state
        .metrics
        .http_requests_total
        .with_label_values(&[&route, &method, &status])
        .inc();
    response
}

async fn metrics_handler(
    State(state): State<AppState>,
) -> Result<(HeaderMap, String), (StatusCode, String)> {
    state
        .metrics
        .render()
        .map(|body| {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                prometheus::TEXT_FORMAT.parse().expect("valid header value"),
            );
            (headers, body)
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

const BANNER: &str = r#"
 __  __  ____ ____
|  \/  |/ ___|  _ \
| |\/| | |   | |_) |
| |  | | |___|  __/
|_|  |_|\____|_|

  mcp_info_server -- Cooper Lees' public MCP endpoint  ::  cooperlees.com

  Production Engineer (SRE) turned MCP author. Written in Rust, deployed
  over IPv6 wherever possible, no auth required -- the content was
  already public. G'day from 🇦🇺.

  Routes:
    POST /mcp             MCP tools: list_posts, get_post, list_pages,
                           get_page, get_resume, list_countdowns,
                           get_countdown
    GET  /coopers-resume   Cooper's resume, rendered as Markdown
                           (alias: /resume)
    GET  /healthz          Liveness check
    GET  /metrics          Prometheus metrics

  https://github.com/cooperlees/mcp_info_server
"#;

async fn root() -> (axum::http::HeaderMap, &'static str) {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "text/plain; charset=utf-8"
            .parse()
            .expect("valid header value"),
    );
    (headers, BANNER)
}

async fn healthz() -> (StatusCode, &'static str) {
    (StatusCode::OK, "Ok")
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::warn!("failed to install SIGTERM handler: {e}"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
