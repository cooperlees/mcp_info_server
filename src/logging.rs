use std::io::IsTerminal;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithTonicConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use tracing_glog::Glog;
use tracing_glog::GlogFields;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

/// Bound on both the TCP connect and the RPC call for the OTLP exporter.
/// Deliberately tight: `OTLP_ENDPOINT` may reach its collector/backend over
/// a slow or unreliable link depending on deployment, and this server's own
/// request handling must never wait on that link being up.
const OTLP_EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Installs the process-wide `tracing` subscriber: glog-style output
/// (`Immdd hh:mm:ss.uuuuuu pid file:line] message`, e.g. what Google's C++/
/// Python logging libraries produce) on stderr. `RUST_LOG` controls
/// verbosity the usual `tracing_subscriber::EnvFilter` way — unset defaults
/// to `info`, which includes startup/shutdown and one `http_request`
/// access-log line per request (see `main.rs::track_http_metrics`);
/// `RUST_LOG=debug` (or e.g. `RUST_LOG=mcp_info_server=debug`) additionally
/// surfaces per-request/tool-call spans, cache hit/miss detail, and a
/// `close` line with `time.busy`/`time.idle` for every span.
///
/// If `OTLP_ENDPOINT` is set (e.g. `http://[fd00:251::26]:4317` for the
/// Tempo instance this deploys with, though any OTLP/gRPC-compatible
/// collector works), every span this server already builds via `tracing`
/// is additionally exported over OTLP/gRPC — see `otlp_tracer_provider` for
/// the non-blocking/failure-tolerant details. Returns the
/// `SdkTracerProvider` in that case so `main.rs` can flush it on graceful
/// shutdown; `None` when unset, which is the default (tracing export is
/// entirely opt-in, like every other env var here).
pub fn init() -> Option<SdkTracerProvider> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Per-layer filtering (not one shared global filter): console output
    // stays governed by RUST_LOG as before, but the otel layer below gets
    // its own independent filter, so enabling OTLP export doesn't require
    // also cranking up RUST_LOG (and therefore Loki noise) just to get the
    // debug_span!-level request/tool-call spans that make traces worth
    // looking at in the first place.
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_span_events(FmtSpan::CLOSE)
        .event_format(Glog::default().with_timer(tracing_glog::LocalTime::default()))
        .fmt_fields(GlogFields::default())
        .with_filter(filter);

    let tracer_provider = otlp_tracer_provider();
    let otel_layer = tracer_provider.clone().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer(env!("CARGO_PKG_NAME")))
            .with_filter(EnvFilter::new("debug"))
    });

    tracing_subscriber::registry()
        .with(fmt)
        .with(otel_layer)
        .init();

    tracer_provider
}

/// Builds an OTLP-bound tracer provider from `OTLP_ENDPOINT`, or `None` if
/// it's unset (checked here, not threaded through `Config`, so this module
/// stays self-contained the same way it already owns `RUST_LOG`).
///
/// Every failure mode here is designed to never touch the request path:
/// - Spans are hand off to an in-memory bounded queue and exported by a
///   dedicated background task (`opentelemetry_sdk::runtime::Tokio`) on its
///   own schedule, never inline with request handling. A full queue just
///   drops the oldest span rather than blocking a producer, per the SDK's
///   standard behavior.
/// - Both the TCP connect and the RPC call are bounded by
///   `OTLP_EXPORT_TIMEOUT` — the connect bound matters specifically because
///   `opentelemetry-otlp`'s own `with_endpoint()`/`with_timeout()` only ever
///   set the RPC-call timeout, never `connect_timeout()`, so a true network
///   black hole (a dead link — not connection-refused, which fails fast on
///   its own) would otherwise hang on tonic/hyper's default (~30s) before
///   the RPC-level timeout even applies. Building the `Channel` by hand is
///   the only way to set both.
/// - A malformed endpoint or exporter construction failure is logged once,
///   here, and just disables OTLP export for this process rather than
///   failing startup — it uses `eprintln!`, not `tracing::warn!`, because
///   this runs before the `tracing` subscriber this function is building is
///   installed.
fn otlp_tracer_provider() -> Option<SdkTracerProvider> {
    let endpoint = std::env::var("OTLP_ENDPOINT").ok()?;

    let channel = match tonic::transport::Endpoint::from_shared(endpoint) {
        Ok(endpoint) => endpoint
            .connect_timeout(OTLP_EXPORT_TIMEOUT)
            .timeout(OTLP_EXPORT_TIMEOUT)
            .connect_lazy(),
        Err(e) => {
            eprintln!("OTLP_ENDPOINT is not a valid URI, disabling OTLP export: {e}");
            return None;
        }
    };

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_channel(channel)
        .build()
    {
        Ok(exporter) => exporter,
        Err(e) => {
            eprintln!("failed to build OTLP exporter, disabling OTLP export: {e}");
            return None;
        }
    };

    let resource = Resource::builder()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();

    let batch = BatchSpanProcessor::builder(exporter, runtime::Tokio).build();

    Some(
        SdkTracerProvider::builder()
            .with_resource(resource)
            .with_span_processor(batch)
            .build(),
    )
}
