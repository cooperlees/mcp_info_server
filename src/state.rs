use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;

use crate::metrics::Metrics;
use crate::resume::ResumeDocument;

/// Errors surfaced by fetch/convert paths, always rendered to callers as plain text.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("upstream request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("upstream returned malformed JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HTML conversion failed: {0}")]
    HtmlConvert(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

impl AppError {
    /// A low-cardinality label for the `mcp_info_server_errors_total` metric.
    pub fn kind(&self) -> &'static str {
        match self {
            AppError::Request(_) => "request",
            AppError::Json(_) => "json",
            AppError::HtmlConvert(_) => "html_convert",
            AppError::NotFound(_) => "not_found",
            AppError::Other(_) => "other",
        }
    }
}

/// Unwrap the `Arc<AppError>` moka's `try_get_with` returns on a failed cache
/// fill. Concurrent callers racing the same key share one `Arc`; whichever
/// caller isn't the sole owner gets a re-stringified copy instead of the
/// original error value (which can't be cloned or moved out from behind the
/// `Arc`).
pub(crate) fn unwrap_cache_error(err: Arc<AppError>) -> AppError {
    Arc::try_unwrap(err).unwrap_or_else(|shared| AppError::Other(shared.to_string()))
}

const HTTP_CACHE_TTL: Duration = Duration::from_secs(45 * 60);
const RESUME_CACHE_TTL: Duration = Duration::from_secs(60);
/// Countdown data changes every second by design (`seconds_remaining`), so it
/// gets a much shorter cache than the WordPress/resume content — long enough
/// to absorb a burst of calls, short enough that "how long until X" stays
/// meaningfully live.
const COUNTDOWN_CACHE_TTL: Duration = Duration::from_secs(30);

/// Shared server state: one HTTP client, a cache of raw upstream response
/// bodies (WordPress JSON, the resume HTML export) keyed by URL, a
/// short-lived cache of the fully-rendered resume document, and an even
/// shorter-lived cache of the countdown API's raw JSON.
#[derive(Clone)]
pub struct AppState {
    pub http: reqwest::Client,
    pub wordpress_url: String,
    pub resume_doc_id: String,
    pub countdown_url: String,
    pub metrics: Metrics,
    resume_base_url: String,
    http_cache: Cache<String, Arc<str>>,
    pub(crate) resume_cache: Cache<(), ResumeDocument>,
    countdown_cache: Cache<String, Arc<str>>,
}

impl AppState {
    pub fn new(
        wordpress_url: String,
        resume_doc_id: String,
        countdown_url: String,
    ) -> Result<Self, AppError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mcp_info_server/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            wordpress_url,
            resume_doc_id,
            countdown_url,
            metrics: Metrics::new(),
            resume_base_url: "https://docs.google.com".to_owned(),
            http_cache: Cache::builder().time_to_live(HTTP_CACHE_TTL).build(),
            resume_cache: Cache::builder().time_to_live(RESUME_CACHE_TTL).build(),
            countdown_cache: Cache::builder().time_to_live(COUNTDOWN_CACHE_TTL).build(),
        })
    }

    pub(crate) fn resume_base_url(&self) -> &str {
        &self.resume_base_url
    }

    /// Point the resume fetch at a different base URL (a local mock server).
    #[cfg(test)]
    pub(crate) fn with_resume_base_url(mut self, base: String) -> Self {
        self.resume_base_url = base;
        self
    }

    /// Fetch `url`'s body as text, serving from cache when available.
    pub async fn fetch_cached(&self, url: &str) -> Result<Arc<str>, AppError> {
        let http = self.http.clone();
        let url_owned = url.to_owned();
        let entry = self
            .http_cache
            .entry(url_owned.clone())
            .or_try_insert_with(async move {
                let body = http
                    .get(&url_owned)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?;
                Ok::<Arc<str>, AppError>(Arc::from(body))
            })
            .await
            .map_err(unwrap_cache_error)?;
        self.metrics.record_cache("http", !entry.is_fresh());
        Ok(entry.into_value())
    }

    /// Fetch the countdown API's JSON body, serving from the short-lived
    /// countdown cache when available. Separate from `fetch_cached` because
    /// this upstream content-negotiates on `Accept` (it serves an HTML page
    /// by default) and needs a much shorter TTL to stay meaningfully live.
    pub async fn fetch_countdown_cached(&self, url: &str) -> Result<Arc<str>, AppError> {
        let http = self.http.clone();
        let url_owned = url.to_owned();
        let entry = self
            .countdown_cache
            .entry(url_owned.clone())
            .or_try_insert_with(async move {
                let body = http
                    .get(&url_owned)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?;
                Ok::<Arc<str>, AppError>(Arc::from(body))
            })
            .await
            .map_err(unwrap_cache_error)?;
        self.metrics.record_cache("countdown", !entry.is_fresh());
        Ok(entry.into_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_cached_serves_repeat_requests_from_cache() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/thing")
            .with_status(200)
            .with_body("hello")
            .expect(1)
            .create_async()
            .await;

        let state = AppState::new(server.url(), "unused".to_owned(), "unused".to_owned()).unwrap();
        let url = format!("{}/thing", server.url());

        let first = state.fetch_cached(&url).await.unwrap();
        let second = state.fetch_cached(&url).await.unwrap();

        assert_eq!(&*first, "hello");
        assert_eq!(&*second, "hello");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn fetch_cached_surfaces_http_errors() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/missing")
            .with_status(404)
            .create_async()
            .await;

        let state = AppState::new(server.url(), "unused".to_owned(), "unused".to_owned()).unwrap();
        let url = format!("{}/missing", server.url());

        let err = state.fetch_cached(&url).await.unwrap_err();
        assert!(matches!(err, AppError::Other(_) | AppError::Request(_)));
    }

    #[tokio::test]
    async fn fetch_countdown_cached_sends_accept_json_and_serves_repeats_from_cache() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/")
            .match_header("accept", "application/json")
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;

        let state = AppState::new("unused".to_owned(), "unused".to_owned(), server.url()).unwrap();
        let url = format!("{}/", server.url());

        let first = state.fetch_countdown_cached(&url).await.unwrap();
        let second = state.fetch_countdown_cached(&url).await.unwrap();

        assert_eq!(&*first, "{}");
        assert_eq!(&*second, "{}");
        mock.assert_async().await;
    }
}
