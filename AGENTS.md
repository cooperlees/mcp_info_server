# AGENTS.md

This file provides guidance to AI coding assistants (Claude Code, OpenAI Codex CLI, Gemini CLI, etc.)
when working with code in this repository. `CLAUDE.md` is a symlink to this file, since Claude Code
doesn't read `AGENTS.md` natively (yet) — keep it that way rather than forking the two.

## Project Overview

`mcp_info_server` is a small Rust/axum server exposing [cooperlees.com](https://cooperlees.com)'s
blog posts, pages, and Cooper's resume two ways: as MCP tools (Streamable HTTP transport, via
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)) and as plain HTTP routes. It's fully
public and unauthenticated — the content it serves is already public. Deployed at
https://mcp.cooperlees.com by the `mcp_info_server` role in the sibling
[clc_ansible](https://github.com/cooperlees/clc_ansible) repo; see that repo for deployment changes,
this one for the server itself. See `README.md` for the full API reference and architecture.

## Build & Development Commands

```bash
cargo build                                            # Debug build
cargo build --release --all-features                  # Release build (matches CI)
cargo test                                             # Run all tests
cargo clippy --all-targets --all-features -- -D warnings  # Lint (matches CI, warnings fail)
cargo fmt --check                                      # Format check (matches CI)
cargo fmt                                               # Auto-format
cargo run                                               # Run locally on :6969
docker build -t mcp_info_server:latest .
docker run --rm -p 6969:6969 mcp_info_server:latest
npx @modelcontextprotocol/inspector http://localhost:6969/mcp   # Interactive MCP debugger
npx @modelcontextprotocol/inspector --cli http://localhost:6969/mcp \
  --method tools/list                                    # Non-interactive - same command CI runs
                                                          # against the release binary, see below

# Release regression gate — wire-level MCP protocol/security checks `cargo test`'s unit
# tests can't catch (they exercise Rust internals, not the actual JSON-RPC/HTTP contract
# a client sees). Runs as part of `cargo test` already; call out explicitly before/after
# a release:
cargo test --test protocol_smoke                        # Pre-release: spawns the just-built binary
MCP_SMOKE_TEST_URL=https://mcp.cooperlees.com \
  cargo test --test protocol_smoke                      # Post-release: checks the live deployment instead
```

All four `cargo` checks (fmt, clippy, test, release build) must pass locally before pushing — this
mirrors `.github/workflows/ci.yml` and `clippy.yml` exactly, so a clean local run means CI will
pass. `ci.yml` also runs that `--cli --method tools/list` Inspector command above against the
just-built release binary and fails if any of the seven tools are missing — an independent client
implementation actually driving this server, not just `protocol_smoke`'s own `reqwest` harness. If
it passes locally against `cargo run`, it'll pass there too — this is reproducibility by
construction, not by coincidence: don't let the CI step and the documented local command drift
apart.

## Keep the root-route ASCII banner in sync

`src/main.rs`'s `BANNER` const (served at `GET /`) lists every MCP tool and every plain HTTP route.
**Whenever you add, rename, or remove an MCP tool (in `src/mcp_server.rs`) or an HTTP route (in
`src/main.rs`'s router), update `BANNER` in the same change** so the two never drift apart. Keep the
existing tone: the terse "MCP" figlet header, a one-line Cooper-Lees-flavored intro (SRE, Rust, IPv6,
public/no-auth), then a plain `Routes:` list — no extra decorative ASCII beyond the header, it reads
better plain.

## Keep new tools/routes instrumented

Every MCP tool call and every HTTP route already gets metrics for free — a new tool added inside
the `#[tool_router] impl InfoServer` block only needs to call `self.instrumented("tool_name", ...)`
(see any existing tool method in `src/mcp_server.rs`) to get `tool_calls_total` and
`tool_call_duration_seconds` with zero extra code; a new plain HTTP route only needs `.route(...)`
added before the existing `.route_layer(...)` call in `main.rs` to get `http_requests_total` and
`http_request_duration_seconds` automatically. **Don't hand-roll per-endpoint counters** — if the
existing helper/middleware doesn't cover a new case, extend it rather than adding a one-off. Also
update the metrics table in `README.md`'s Metrics section and, if it's a new cache, wire it through
`Metrics::record_cache` the same way `state.rs`/`resume.rs` do.

## Architecture

```
src/main.rs           axum wiring, config from env, graceful shutdown, the one HTTP-metrics
                      middleware layer covering every route
src/state.rs           AppState (reqwest client + caches + Metrics), AppError
src/metrics.rs          Prometheus registry + counters/histograms, /metrics render
src/mcp_server.rs       InfoServer — the #[tool_router] exposing the MCP tools, each one line
                       of `self.instrumented("name", ...)`
src/wordpress.rs        WP REST API client + typed post/page structs
src/resume.rs           Google Doc fetch + table-walk → Markdown conversion
src/html_convert.rs      shared HTML cleanup (strip hidden/decorative elements, unwrap Google
                        redirect links) + htmd Markdown conversion
src/resume_route.rs      plain GET /coopers-resume handler, reuses resume.rs
src/countdown.rs         countdown.cooperlees.com JSON API client + typed structs
```

## Conventions

- No bare `.unwrap()` outside `#[cfg(test)]` code. Use `?` with `AppError` (thiserror, in
  `state.rs`) for anything genuinely fallible; `.expect("...")` only for invariants that can't
  actually fail at runtime (a hardcoded `Selector::parse` call, a static header value).
- CPU-bound work (HTML parsing, Markdown conversion) runs via `tokio::task::spawn_blocking` — never
  inline in an async fn. See `resume.rs::get_resume` and `wordpress.rs::convert_blocking`.
- Tests are colocated (`#[cfg(test)] mod tests` in the module they cover), not in a separate
  `tests/` integration suite (except the one real fixture, `tests/fixtures/resume_export.html`).
  Mock HTTP with `mockito`, not a live network call.
- Config is env vars only (see README's Configuration table), all with working defaults — no
  required config for local dev.
- Always move to the latest stable Rust language edition / toolchain features when they're stable;
  don't hold back for compatibility this project doesn't need.
