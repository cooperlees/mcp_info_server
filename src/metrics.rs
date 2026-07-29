use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry, TextEncoder};

use crate::state::AppError;

/// Prometheus metrics for this server. Kept as one struct on `AppState` (not
/// the crate's global default registry) so each `AppState` — including ones
/// built fresh in tests — owns an independent set of counters instead of
/// sharing one global, mutable registry across the process.
///
/// Raw counters and histograms only: rate/aggregation over any time window
/// (1m, 5m, 1h, ...) is Prometheus's job at query time (`rate(...[5m])`),
/// not something to pre-compute and export as separate metrics here.
#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    pub http_requests_total: IntCounterVec,
    pub http_request_duration_seconds: HistogramVec,
    pub client_subnet_requests_total: IntCounterVec,
    pub tool_calls_total: IntCounterVec,
    pub tool_call_duration_seconds: HistogramVec,
    cache_requests_total: IntCounterVec,
    errors_total: IntCounterVec,
}

fn register<T: prometheus::core::Collector + Clone + 'static>(registry: &Registry, metric: &T) {
    registry
        .register(Box::new(metric.clone()))
        .expect("metric name is unique within this registry");
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let http_requests_total = IntCounterVec::new(
            Opts::new(
                "mcp_info_server_http_requests_total",
                "Total HTTP requests, by route, method, and status code",
            ),
            &["route", "method", "status"],
        )
        .expect("valid metric definition");
        register(&registry, &http_requests_total);

        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "mcp_info_server_http_request_duration_seconds",
                "HTTP request latency in seconds, by route and method",
            ),
            &["route", "method"],
        )
        .expect("valid metric definition");
        register(&registry, &http_request_duration_seconds);

        // client_ip itself is never a label — unbounded on a public,
        // unauthenticated endpoint. The containing /24 (v4) or /64 (v6) is a
        // bounded, meaningful grouping instead (see main.rs::client_subnet).
        let client_subnet_requests_total = IntCounterVec::new(
            Opts::new(
                "mcp_info_server_client_subnet_requests_total",
                "Total HTTP requests by client subnet (/24 for IPv4, /64 for IPv6)",
            ),
            &["subnet"],
        )
        .expect("valid metric definition");
        register(&registry, &client_subnet_requests_total);

        let tool_calls_total = IntCounterVec::new(
            Opts::new(
                "mcp_info_server_tool_calls_total",
                "Total MCP tool calls, by tool name and result (ok/error)",
            ),
            &["tool", "result"],
        )
        .expect("valid metric definition");
        register(&registry, &tool_calls_total);

        let tool_call_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "mcp_info_server_tool_call_duration_seconds",
                "MCP tool call latency in seconds, by tool name",
            ),
            &["tool"],
        )
        .expect("valid metric definition");
        register(&registry, &tool_call_duration_seconds);

        let cache_requests_total = IntCounterVec::new(
            Opts::new(
                "mcp_info_server_cache_requests_total",
                "Total cache lookups, by cache name and result (hit/miss)",
            ),
            &["cache", "result"],
        )
        .expect("valid metric definition");
        register(&registry, &cache_requests_total);

        let errors_total = IntCounterVec::new(
            Opts::new(
                "mcp_info_server_errors_total",
                "Total errors, by AppError kind",
            ),
            &["kind"],
        )
        .expect("valid metric definition");
        register(&registry, &errors_total);

        Self {
            registry,
            http_requests_total,
            http_request_duration_seconds,
            client_subnet_requests_total,
            tool_calls_total,
            tool_call_duration_seconds,
            cache_requests_total,
            errors_total,
        }
    }

    /// Record a cache lookup outcome. `hit = true` means the value was
    /// already cached; `hit = false` means this call triggered a fresh fetch.
    pub fn record_cache(&self, cache: &str, hit: bool) {
        self.cache_requests_total
            .with_label_values(&[cache, if hit { "hit" } else { "miss" }])
            .inc();
    }

    pub fn record_error(&self, err: &AppError) {
        self.errors_total.with_label_values(&[err.kind()]).inc();
    }

    /// Render all registered metrics in Prometheus text exposition format.
    pub fn render(&self) -> Result<String, AppError> {
        let families = self.registry.gather();
        let mut buf = String::new();
        TextEncoder::new()
            .encode_utf8(&families, &mut buf)
            .map_err(|e| AppError::Other(format!("failed to encode metrics: {e}")))?;
        Ok(buf)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_all_metric_families() {
        let metrics = Metrics::new();
        metrics
            .http_requests_total
            .with_label_values(&["/healthz", "GET", "200"])
            .inc();
        metrics
            .client_subnet_requests_total
            .with_label_values(&["203.0.113.0/24"])
            .inc();
        metrics.record_cache("http", true);
        metrics.record_cache("http", false);
        metrics.record_error(&AppError::NotFound("nope".to_owned()));

        let body = metrics.render().unwrap();
        assert!(body.contains("mcp_info_server_http_requests_total"));
        assert!(body.contains(
            r#"mcp_info_server_client_subnet_requests_total{subnet="203.0.113.0/24"} 1"#
        ));
        assert!(body.contains("mcp_info_server_cache_requests_total"));
        assert!(body.contains("mcp_info_server_errors_total"));
        assert!(body.contains(r#"kind="not_found""#));
    }

    #[test]
    fn record_cache_labels_hit_and_miss_distinctly() {
        let metrics = Metrics::new();
        metrics.record_cache("resume", true);
        metrics.record_cache("resume", false);
        metrics.record_cache("resume", false);

        let body = metrics.render().unwrap();
        assert!(
            body.contains(r#"mcp_info_server_cache_requests_total{cache="resume",result="hit"} 1"#)
        );
        assert!(
            body.contains(
                r#"mcp_info_server_cache_requests_total{cache="resume",result="miss"} 2"#
            )
        );
    }

    #[test]
    fn two_independent_instances_dont_panic_on_duplicate_registration() {
        // Regression check for the reason this isn't built on the crate's
        // global default registry: constructing two Metrics (as two AppStates
        // in two tests, or two requests in one test) must not collide.
        let _a = Metrics::new();
        let _b = Metrics::new();
    }
}
