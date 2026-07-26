mod html_convert;
mod mcp_server;
mod resume;
mod resume_route;
mod state;
mod wordpress;

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

use crate::mcp_server::InfoServer;
use crate::state::{AppError, AppState};

const DEFAULT_WORDPRESS_URL: &str = "https://cooperlees.com";
const DEFAULT_RESUME_DOC_ID: &str = "1ksWGBa1ZrGVItQybR-tVnism2E-LIGsmcdf-CDalFcw";
const DEFAULT_LISTEN_PORT: u16 = 6969;
/// rmcp's Streamable HTTP transport rejects requests whose `Host` header
/// isn't in this list (DNS-rebinding protection) — it defaults to loopback
/// only, so a public deployment behind Traefik must add its own hostname via
/// `ALLOWED_HOSTS`, or every real request gets a 403.
const DEFAULT_ALLOWED_HOSTS: &str = "localhost,127.0.0.1,::1";

struct Config {
    wordpress_url: String,
    resume_doc_id: String,
    listen_port: u16,
    allowed_hosts: Vec<String>,
}

impl Config {
    fn from_env() -> Result<Self, AppError> {
        let wordpress_url =
            std::env::var("WORDPRESS_URL").unwrap_or_else(|_| DEFAULT_WORDPRESS_URL.to_owned());
        let resume_doc_id =
            std::env::var("RESUME_DOC_ID").unwrap_or_else(|_| DEFAULT_RESUME_DOC_ID.to_owned());
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
    let state = AppState::new(config.wordpress_url.clone(), config.resume_doc_id.clone())?;

    let mcp_state = state.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(InfoServer::new(mcp_state.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_allowed_hosts(config.allowed_hosts.clone()),
    );

    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .route("/coopers-resume", get(resume_route::coopers_resume))
        .route("/healthz", get(healthz))
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

async fn healthz() -> StatusCode {
    StatusCode::OK
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
