use std::io::IsTerminal;

use tracing_glog::Glog;
use tracing_glog::GlogFields;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

/// Installs the process-wide `tracing` subscriber: glog-style output
/// (`Immdd hh:mm:ss.uuuuuu pid file:line] message`, e.g. what Google's C++/
/// Python logging libraries produce) on stderr. `RUST_LOG` controls
/// verbosity the usual `tracing_subscriber::EnvFilter` way — unset defaults
/// to `info`, which includes startup/shutdown and one `http_request`
/// access-log line per request (see `main.rs::track_http_metrics`);
/// `RUST_LOG=debug` (or e.g. `RUST_LOG=mcp_info_server=debug`) additionally
/// surfaces per-request/tool-call spans, cache hit/miss detail, and a
/// `close` line with `time.busy`/`time.idle` for every span.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_span_events(FmtSpan::CLOSE)
        .event_format(Glog::default().with_timer(tracing_glog::LocalTime::default()))
        .fmt_fields(GlogFields::default());

    tracing_subscriber::registry().with(filter).with(fmt).init();
}
