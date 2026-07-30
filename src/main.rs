mod asn;
mod countdown;
mod html_convert;
mod logging;
mod mcp_server;
mod metrics;
mod rate_limit;
mod resume;
mod resume_route;
mod state;
mod wordpress;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, MatchedPath, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use tracing::Instrument;

use crate::mcp_server::InfoServer;
use crate::rate_limit::{Allowlist, RateLimitConfig};
use crate::state::{AppError, AppState};

const DEFAULT_WORDPRESS_URL: &str = "https://cooperlees.com";
const DEFAULT_RESUME_DOC_ID: &str = "1ksWGBa1ZrGVItQybR-tVnism2E-LIGsmcdf-CDalFcw";
const DEFAULT_COUNTDOWN_URL: &str = "https://countdown.cooperlees.com";
/// The Prometheus this server's own metrics get scraped by — same Docker
/// network in the real deployment, resolved by container name, so this
/// default just works there. Used to skip a Cymru DNS round trip when a
/// client subnet's ASN is already sitting in Prometheus from a prior
/// process lifetime (see `asn::lookup_from_prometheus`).
const DEFAULT_PROMETHEUS_URL: &str = "http://prometheus:9090";
const DEFAULT_LISTEN_PORT: u16 = 6969;
/// rmcp's Streamable HTTP transport rejects requests whose `Host` header
/// isn't in this list (DNS-rebinding protection) — it defaults to loopback
/// only, so a public deployment behind Traefik must add its own hostname via
/// `ALLOWED_HOSTS`, or every real request gets a 403.
const DEFAULT_ALLOWED_HOSTS: &str = "localhost,127.0.0.1,::1";
/// Sustained requests/second and burst allowance per client *subnet* (not
/// exact IP — see `rate_limit.rs`) before `track_http_metrics` starts
/// returning 429. Generous enough for a normal handful of rapid MCP tool
/// calls in one session; still bounds a runaway script to a two-digit
/// requests/minute ceiling.
const DEFAULT_RATE_LIMIT_PER_SECOND: u32 = 5;
const DEFAULT_RATE_LIMIT_BURST: u32 = 20;
/// How often to drop rate-limiter state for subnets that have gone idle —
/// not configurable, this is purely an internal memory-bound housekeeping
/// detail, not a rate-limiting behavior. See
/// `AppState::rate_limiter_gc_tick`.
const RATE_LIMITER_GC_INTERVAL: Duration = Duration::from_secs(5 * 60);

struct Config {
    wordpress_url: String,
    resume_doc_id: String,
    countdown_url: String,
    prometheus_url: String,
    listen_port: u16,
    allowed_hosts: Vec<String>,
    rate_limit: RateLimitConfig,
}

impl Config {
    fn from_env() -> Result<Self, AppError> {
        let wordpress_url =
            std::env::var("WORDPRESS_URL").unwrap_or_else(|_| DEFAULT_WORDPRESS_URL.to_owned());
        let resume_doc_id =
            std::env::var("RESUME_DOC_ID").unwrap_or_else(|_| DEFAULT_RESUME_DOC_ID.to_owned());
        let countdown_url =
            std::env::var("COUNTDOWN_URL").unwrap_or_else(|_| DEFAULT_COUNTDOWN_URL.to_owned());
        let prometheus_url =
            std::env::var("PROMETHEUS_URL").unwrap_or_else(|_| DEFAULT_PROMETHEUS_URL.to_owned());
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
        let rate_limit = RateLimitConfig {
            per_second: env_nonzero_u32("RATE_LIMIT_PER_SECOND", DEFAULT_RATE_LIMIT_PER_SECOND)?,
            burst: env_nonzero_u32("RATE_LIMIT_BURST", DEFAULT_RATE_LIMIT_BURST)?,
            allowlist: Allowlist::parse(&std::env::var("RATE_LIMIT_ALLOWLIST").unwrap_or_default())
                .map_err(|e| AppError::Other(format!("RATE_LIMIT_ALLOWLIST: {e}")))?,
        };

        Ok(Self {
            wordpress_url,
            resume_doc_id,
            countdown_url,
            prometheus_url,
            listen_port,
            allowed_hosts,
            rate_limit,
        })
    }
}

fn env_nonzero_u32(var: &str, default: u32) -> Result<NonZeroU32, AppError> {
    let raw = match std::env::var(var) {
        Ok(raw) => raw,
        Err(_) => {
            return Ok(
                NonZeroU32::new(default).expect("DEFAULT_RATE_LIMIT_* constants are non-zero")
            );
        }
    };
    let parsed: u32 = raw
        .parse()
        .map_err(|e| AppError::Other(format!("{var} {raw:?} is not a valid number: {e}")))?;
    NonZeroU32::new(parsed).ok_or_else(|| AppError::Other(format!("{var} must be greater than 0")))
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    let tracer_provider = logging::init();

    let config = Config::from_env()?;
    tracing::debug!(
        wordpress_url = %config.wordpress_url,
        resume_doc_id = %config.resume_doc_id,
        countdown_url = %config.countdown_url,
        prometheus_url = %config.prometheus_url,
        listen_port = config.listen_port,
        allowed_hosts = ?config.allowed_hosts,
        rate_limit_per_second = config.rate_limit.per_second.get(),
        rate_limit_burst = config.rate_limit.burst.get(),
        "config loaded",
    );
    let state = AppState::new(
        config.wordpress_url.clone(),
        config.resume_doc_id.clone(),
        config.countdown_url.clone(),
        config.prometheus_url.clone(),
        config.rate_limit.clone(),
    )?;

    tokio::spawn({
        let state = state.clone();
        async move {
            let mut interval = tokio::time::interval(RATE_LIMITER_GC_INTERVAL);
            loop {
                interval.tick().await;
                state.rate_limiter_gc_tick();
            }
        }
    });

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

    // `::` dual-stack binds both IPv6 and IPv4 (via IPv4-mapped addresses) on
    // one socket — Linux's default unless `net.ipv6.bindv6only` is set.
    let addr = format!("[::]:{}", config.listen_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| AppError::Other(format!("failed to bind {addr}: {e}")))?;

    tracing::info!(%addr, "mcp_info_server listening");

    let serve_result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await;

    // Flush any spans still sitting in the batch queue before exiting,
    // regardless of how serve() ended - best-effort, never fatal.
    if let Some(provider) = tracer_provider
        && let Err(e) = provider.shutdown()
    {
        tracing::warn!(error = %e, "failed to flush Jaeger tracer provider on shutdown");
    }

    serve_result.map_err(|e| AppError::Other(format!("server error: {e}")))?;

    Ok(())
}

/// Traefik sits in front of every real request and sets `X-Forwarded-For`
/// (leftmost entry is the original client) — trust it over the TCP peer
/// address, which is always Traefik's own container IP. Falls back to the
/// peer address for anything that reaches the server directly (local dev,
/// healthchecks).
fn client_ip(req: &Request<axum::body::Body>, peer_addr: SocketAddr) -> String {
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| peer_addr.ip().to_string())
}

/// A dual-stack `[::]` listener hands IPv4 peers to us as IPv4-mapped IPv6
/// addresses (`::ffff:a.b.c.d`) — `IpAddr::from_str` parses that as `V6`,
/// not `V4`, so anything downstream that branches on the variant (subnet
/// masking, `asn::is_publicly_routable`, the Cymru query builder) needs this
/// run first or it'll treat a plain private v4 peer as some exotic public
/// v6 address.
fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// Coarsens a client address to /24 (v4) or /64 (v6) — the bounded grouping
/// used as an actual Prometheus label, since the raw address is unbounded on
/// a public endpoint. Also the semantically right grouping for IPv6: RFC
/// 4941 privacy addresses mean the same client can present a different host
/// part within its /64 on every connection.
fn client_subnet(ip: &str) -> String {
    match ip.parse::<IpAddr>().map(normalize_ip) {
        Ok(IpAddr::V4(v4)) => ipv4_slash24(v4),
        Ok(IpAddr::V6(v6)) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        Err(_) => "unknown".to_owned(),
    }
}

fn ipv4_slash24(v4: Ipv4Addr) -> String {
    let o = v4.octets();
    format!("{}.{}.{}.0/24", o[0], o[1], o[2])
}

/// A `Retry-After` is required by nothing here, but it's the one piece of
/// information a well-behaved caller actually wants out of a 429: exactly
/// how long to back off. Rounded up to whole seconds (and at least 1) so a
/// caller that waits exactly the stated time never retries too early.
fn rate_limited_response(retry_after: Duration) -> Response {
    let retry_after_secs = retry_after.as_secs_f64().ceil().max(1.0) as u64;
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(
            axum::http::header::RETRY_AFTER,
            retry_after_secs.to_string(),
        )],
        "rate limit exceeded\n",
    )
        .into_response()
}

/// The current span's OpenTelemetry trace ID, formatted the same way
/// Jaeger displays/searches it (32 lowercase hex chars) — lets a human
/// jump from a Loki log line straight to the matching Jaeger trace,
/// wired via Grafana's Loki datasource `derivedFields` config. `"unknown"`
/// whenever `JAEGER_OTLP_ENDPOINT` is unset, since then there's no real
/// OTel context to report (the span still exists for the `RUST_LOG=debug`
/// span tree, it's just never exported).
fn current_trace_id() -> String {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let span_context = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .clone();
    if span_context.is_valid() {
        span_context.trace_id().to_string()
    } else {
        "unknown".to_owned()
    }
}

/// Records `mcp_info_server_http_requests_total` and
/// `_http_request_duration_seconds` for every request, labeled by the
/// matched route template (never the raw path — `/mcp` carries every MCP
/// tool call, so per-tool breakdowns come from `mcp_server.rs`'s own
/// instrumentation instead of by route here).
///
/// The raw client address deliberately isn't a metric label (unbounded
/// cardinality on a public, unauthenticated endpoint) — every request logs
/// one `info`-level access-log line with the exact `client_ip`, which Loki
/// picks up for per-client breakdowns in Grafana, while a bounded
/// `client_subnet` (see `client_subnet()`) does become a real Prometheus
/// label on `mcp_info_server_client_subnet_requests_total`. A detached task
/// additionally resolves that subnet's ASN (`asn` module) and records it on
/// `mcp_info_server_client_asn_requests_total` once resolution completes —
/// never on the request path itself, and "unknown"/"unknown" until it is.
///
/// Also where rate limiting happens (`AppState::check_rate_limit`, keyed by
/// the same `client_subnet` — see `rate_limit.rs`): a rejected request never
/// reaches `next.run()`, so it never touches an actual handler, but it's
/// still counted in every metric/log line below with `status=429`, same as
/// a normal request.
async fn track_http_metrics(
    State(state): State<AppState>,
    matched_path: Option<MatchedPath>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let method = req.method().to_string();
    let route = matched_path
        .as_ref()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_owned();
    let client_ip = client_ip(&req, peer_addr);
    let span = tracing::debug_span!("http_request", %method, %route, %client_ip);

    async move {
        let start = std::time::Instant::now();
        let subnet = client_subnet(&client_ip);
        let parsed_ip = client_ip.parse::<IpAddr>().map(normalize_ip).ok();
        let retry_after = parsed_ip.and_then(|ip| state.check_rate_limit(ip, &subnet));

        let response = match retry_after {
            Some(retry_after) => rate_limited_response(retry_after),
            None => {
                let timer = state
                    .metrics
                    .http_request_duration_seconds
                    .with_label_values(&[&route, &method])
                    .start_timer();
                let response = next.run(req).await;
                timer.observe_duration();
                response
            }
        };
        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

        let status = response.status();
        state
            .metrics
            .http_requests_total
            .with_label_values(&[&route, &method, &status.as_u16().to_string()])
            .inc();
        state
            .metrics
            .client_subnet_requests_total
            .with_label_values(&[&subnet])
            .inc();
        // ASN resolution can mean two sequential DNS round trips (Cymru) -
        // never worth holding the response up for, so it runs after we've
        // already got `response` in hand, in a detached task. Cheap on a
        // cache hit (the overwhelmingly common case) either way.
        if let Some(ip) = parsed_ip {
            let state = state.clone();
            tokio::spawn(async move {
                let info = state.lookup_asn_cached(&subnet, ip).await;
                let (asn, org) = info
                    .as_deref()
                    .map(|i| (i.asn_label(), i.org.clone()))
                    .unwrap_or_else(|| (asn::UNKNOWN.to_owned(), asn::UNKNOWN.to_owned()));
                state
                    .metrics
                    .client_asn_requests_total
                    .with_label_values(&[&subnet, &asn, &org])
                    .inc();
            });
        }
        tracing::info!(
            %client_ip,
            %method,
            %route,
            status = status.as_u16(),
            duration_ms,
            trace_id = %current_trace_id(),
            "http_request"
        );
        // Deliberately no extra warn! for 429s specifically: unlike the
        // (cached, at-most-once-per-24h) ASN lookup failure log, nothing
        // here is deduplicated - the whole point of a 429 is that it can
        // happen every request during exactly the burst this exists to
        // survive, and a warn! per rejection would turn the protection
        // itself into a logging flood. The existing info-level
        // `http_request` line already carries status=429 for every request,
        // rate limited or not - that's the record.
        if status.is_server_error() {
            tracing::warn!(%status, "request failed");
        } else {
            tracing::debug!(%status, "request completed");
        }
        response
    }
    .instrument(span)
    .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_subnet_masks_ipv4_to_slash24() {
        assert_eq!(client_subnet("203.0.113.42"), "203.0.113.0/24");
    }

    #[test]
    fn client_subnet_masks_ipv6_to_slash64() {
        assert_eq!(
            client_subnet("2600:1702:7310:20e0::69"),
            "2600:1702:7310:20e0::/64"
        );
    }

    #[test]
    fn client_subnet_normalizes_ipv4_mapped_ipv6_to_slash24() {
        assert_eq!(client_subnet("::ffff:10.251.254.20"), "10.251.254.0/24");
    }

    #[test]
    fn client_subnet_falls_back_to_unknown_for_garbage() {
        assert_eq!(client_subnet("not-an-ip"), "unknown");
    }

    #[test]
    fn normalize_ip_unwraps_ipv4_mapped_ipv6() {
        assert_eq!(
            normalize_ip("::ffff:10.251.254.20".parse().unwrap()),
            "10.251.254.20".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn normalize_ip_leaves_plain_addresses_untouched() {
        assert_eq!(
            normalize_ip("2600:1702:7310:20e0::69".parse().unwrap()),
            "2600:1702:7310:20e0::69".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            normalize_ip("203.0.113.42".parse().unwrap()),
            "203.0.113.42".parse::<IpAddr>().unwrap()
        );
    }

    /// Regression test: a private IPv4 peer on a dual-stack `[::]` listener
    /// (e.g. Prometheus scraping /metrics directly, no X-Forwarded-For)
    /// arrives as `::ffff:10.x.x.x`. Without normalizing first,
    /// `is_publicly_routable` sees a `V6` that isn't loopback/ULA/link-local
    /// and wrongly calls it public - this asserts the two functions compose
    /// correctly, the way `track_http_metrics` actually calls them.
    #[test]
    fn normalized_ipv4_mapped_private_address_is_not_publicly_routable() {
        let ip: IpAddr = "::ffff:10.251.254.20".parse().unwrap();
        assert!(!crate::asn::is_publicly_routable(normalize_ip(ip)));
    }
}
