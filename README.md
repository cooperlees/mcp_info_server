# mcp_info_server

[![Rust CI](https://github.com/cooperlees/mcp_info_server/actions/workflows/ci.yml/badge.svg)](https://github.com/cooperlees/mcp_info_server/actions/workflows/ci.yml)
[![Rust Clippy CI](https://github.com/cooperlees/mcp_info_server/actions/workflows/clippy.yml/badge.svg)](https://github.com/cooperlees/mcp_info_server/actions/workflows/clippy.yml)
[![Docker Build + Push](https://github.com/cooperlees/mcp_info_server/actions/workflows/docker.yml/badge.svg)](https://github.com/cooperlees/mcp_info_server/actions/workflows/docker.yml)

A small Rust server that exposes [cooperlees.com](https://cooperlees.com)'s
blog posts, pages, and Cooper's resume two ways from one process: as **MCP
tools** (for LLM clients like Claude) and as a **plain HTTP route** (for
`curl` / sharing a link). Everything it serves is already public content —
the endpoint itself is intentionally unauthenticated.

Deployed at **https://mcp.cooperlees.com** — see
[Deployment](#deployment) for how it gets there.

## What it exposes

### MCP tools (`POST /mcp`, Streamable HTTP transport)

| Tool | Description |
|---|---|
| `list_posts(page?, per_page?)` | Paginated list of blog posts (summary + excerpt as Markdown) |
| `get_post(slug)` | A single post, full content as Markdown |
| `list_pages(page?, per_page?)` | Paginated list of static pages |
| `get_page(slug)` | A single page, full content as Markdown |
| `get_resume()` | Cooper's resume, rendered as Markdown |
| `list_countdowns()` | All of Cooper's [countdown.cooperlees.com](https://countdown.cooperlees.com) events (trips, birthdays, weddings, etc), soonest first, with live time-remaining |
| `get_countdown(slug)` | A single countdown event by slug, with live time-remaining |

Point any Streamable HTTP-capable MCP client at `https://mcp.cooperlees.com/mcp`
— no auth headers, no OAuth handshake required (see
[Connecting an LLM TUI](#connecting-an-llm-tui) below for the exact command
per client).

### Plain HTTP routes

| Route | Description |
|---|---|
| `GET /` | ASCII-art landing page listing available routes/tools |
| `GET /coopers-resume` (alias: `GET /resume`) | The same rendered resume `get_resume` returns, as `text/markdown` — no MCP client needed |
| `GET /healthz` | Liveness check, always `200`, body `Ok` |
| `GET /metrics` | Prometheus metrics (see [Metrics](#metrics)) |

## Connecting an LLM TUI

The server is public and unauthenticated, so every client below just needs
the URL — no token, no header, no OAuth step.

**Claude Code**

```bash
claude mcp add --transport http mcp_info_server https://mcp.cooperlees.com/mcp
```

Verify with `/mcp` inside a session, or `claude mcp list` from the shell.
Start a *new* session after adding it — an already-running one won't pick it
up.

**OpenAI Codex CLI**

```bash
codex mcp add mcp_info_server --url https://mcp.cooperlees.com/mcp
```

Verify with `codex mcp list`, or `/mcp` inside a session. Equivalent manual
config, in `~/.codex/config.toml`:

```toml
[mcp_servers.mcp_info_server]
url = "https://mcp.cooperlees.com/mcp"
```

**Google Gemini CLI**

```bash
gemini mcp add --transport http mcp_info_server https://mcp.cooperlees.com/mcp
```

Equivalent manual config, in `~/.gemini/settings.json`:

```json
{
  "mcpServers": {
    "mcp_info_server": {
      "httpUrl": "https://mcp.cooperlees.com/mcp"
    }
  }
}
```

In any of the three, you don't need to name a tool directly — just ask in
plain language, e.g. *"show me my resume"* or *"what blog posts do you
have?"*, and the client will invoke `get_resume` / `list_posts` on its own.

## How it works

- **WordPress content** (`list_posts`/`get_post`/`list_pages`/`get_page`) is
  fetched from cooperlees.com's stock [WP REST API](https://developer.wordpress.org/rest-api/)
  (`/wp-json/wp/v2/posts`, `/pages`) — no plugin or auth required, since the
  content is already 100% public.
- **The resume** is fetched from its Google Doc's HTML export
  (`/export?format=html`), then converted to Markdown: the raw HTML is
  parsed, decorative spacer `<img>` tags are stripped, `display:none`
  elements are dropped (this WordPress install's Markdown-rendering plugin
  embeds a hidden div with the raw escaped source next to the real content —
  without this step it'd leak into the output), and Google's
  `/url?q=<real>&sa=...` redirect-wrapped links are unwrapped back to their
  real target. See `src/resume.rs` / `src/html_convert.rs`.
- **Countdown data** is fetched from countdown.cooperlees.com's own JSON API
  (it content-negotiates on `Accept: application/json` — no header, and it
  serves its HTML page instead), already pre-computed (`seconds_remaining`,
  a `years/days/hours/minutes/seconds` breakdown, a human string like `"4d to
  go"`) — no conversion needed. See `src/countdown.rs`.
- The WordPress/resume paths share an HTTP-response cache (45 min TTL, keyed
  by URL); the resume additionally has its own fully-rendered-document cache
  (1 min TTL) on top, so a burst of calls — an MCP tool call followed
  moments later by a human hitting `/coopers-resume` — shares one render
  instead of re-fetching/re-parsing each time. Countdown data gets its own
  much shorter cache (30s TTL) instead, since `seconds_remaining` changes
  every second by design and a 45-minute-stale countdown isn't useful.
- HTML parsing and Markdown conversion (CPU-bound) run via
  `tokio::task::spawn_blocking` so they never stall the async runtime under
  load.

## Deployment

Built as a Docker image (`cooperlees/mcp_info_server` on Docker Hub, built
and pushed by [`.github/workflows/docker.yml`](.github/workflows/docker.yml)
on every push to `main`) and deployed by the
[`mcp_info_server` role](https://github.com/cooperlees/clc_ansible/tree/main/roles/mcp_info_server)
in the [clc_ansible](https://github.com/cooperlees/clc_ansible) repo, which
this repo has no dependency on beyond that — it's a plain container that
takes its config from env vars.

```
                          ┌─────────────────────────────┐
  Internet ── HTTPS ──▶   │  Traefik (godaddy certresolver)│
  mcp.cooperlees.com      │  Host(`mcp.cooperlees.com`)  │
                          └──────────────┬───────────────┘
                                         │ routable_net (fd00:251::/64)
                                         ▼
                          ┌─────────────────────────────┐
                          │   mcp_info_server container  │
                          │   :6969 — /mcp /coopers-resume│
                          │   /healthz                    │
                          │   cpus: 1, memory: 256m       │
                          └──────────────┬───────────────┘
                                         │ HTTPS, outbound only
                        ┌────────────────┼────────────────┐
                        ▼                ▼                ▼
               cooperlees.com    docs.google.com   countdown.cooperlees.com
               (WP REST API)     (resume export)   (countdown JSON API)
```

On the VPS it runs on Docker's `routable_net`, routed purely by Traefik
docker labels (`traefik.http.routers.mcp...`) — same pattern as every other
service on that host (`wordpress`, `prometheus`, etc). IPv6 is preferred
throughout this stack wherever it's viable; `routable_net` is dual-stack
(routing + DNS both keep an IPv4 address around for compatibility), but
IPv6 is the addressing shown here since it's what the network is built
around. TLS is issued via the same shared `godaddy` DNS-01 certresolver.
There is deliberately no basicauth middleware in front of it: the content
is public and Streamable HTTP doesn't have a built-in auth mechanism most
MCP clients could negotiate anyway.

## Configuration

All via environment variables, all optional — every one has a working
default:

| Variable | Default | Notes |
|---|---|---|
| `WORDPRESS_URL` | `https://cooperlees.com` | Base URL for the WP REST API |
| `RESUME_DOC_ID` | Cooper's resume doc ID | The Google Doc ID (the `.../d/<ID>/edit` part) |
| `COUNTDOWN_URL` | `https://countdown.cooperlees.com` | Base URL for the countdown JSON API |
| `LISTEN_PORT` | `6969` | |
| `ALLOWED_HOSTS` | `localhost,127.0.0.1,::1` | Comma-separated `Host` header allowlist — rmcp's Streamable HTTP transport rejects any request whose `Host` isn't in this list (DNS-rebinding protection). **A public deployment must add its own hostname here or every real request gets a 403** — the ansible role sets this to `mcp.cooperlees.com,localhost,127.0.0.1,::1`. |

None of this is secret — no vault entry needed for deployment.

## Running locally

```bash
cargo run
# or, against a local WordPress/whatever:
WORDPRESS_URL=http://localhost:8888 cargo run
```

```bash
docker build -t mcp_info_server:latest .
docker run --rm -p 6969:6969 mcp_info_server:latest
curl http://localhost:6969/healthz
curl http://localhost:6969/coopers-resume
```

Point the [MCP inspector](https://github.com/modelcontextprotocol/inspector)
at a running instance:

```bash
npx @modelcontextprotocol/inspector http://localhost:6969/mcp
# or, non-interactively:
npx @modelcontextprotocol/inspector --cli http://localhost:6969/mcp --method tools/list
```

## Testing

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Tests are colocated with the code they cover (`#[cfg(test)] mod tests` in
each module) and run against `mockito`-mocked HTTP, except
`src/resume.rs`'s conversion tests, which run against a real, committed
fixture of the resume's HTML export (`tests/fixtures/resume_export.html`) so
the Markdown-conversion logic is checked against actual Google Docs export
markup, not a hand-simplified stand-in.

## Metrics

`GET /metrics` exposes Prometheus text-format metrics, all prefixed `mcp_info_server_`:

| Metric | Type | Labels | What it's for |
|---|---|---|---|
| `http_requests_total` | counter | `route`, `method`, `status` | Requests per HTTP route (`/`, `/mcp`, `/coopers-resume`, `/healthz`, `/metrics`) |
| `http_request_duration_seconds` | histogram | `route`, `method` | Latency per HTTP route |
| `tool_calls_total` | counter | `tool`, `result` (`ok`/`error`) | Calls per MCP tool — since all 7 tools share the one `/mcp` route, this is where the per-tool breakdown actually lives |
| `tool_call_duration_seconds` | histogram | `tool` | Latency per MCP tool |
| `cache_requests_total` | counter | `cache` (`http`/`resume`/`countdown`), `result` (`hit`/`miss`) | Hit/miss rate for all three cache layers (see [How it works](#how-it-works)) |
| `errors_total` | counter | `kind` (`request`/`json`/`html_convert`/`not_found`/`other`) | Every `AppError`, regardless of whether it surfaced as an HTTP 502 or an MCP tool error |

These are deliberately raw counters/histograms, not pre-aggregated 1-minute/5-minute/1-hour
numbers — that windowing is exactly what PromQL's `rate()`/`increase()` do at query time
(`rate(mcp_info_server_tool_calls_total[5m])`), so the Grafana dashboard has one panel per
metric with the range picker doing the window selection, rather than three near-duplicate
exported metrics per thing being measured.

## Architecture

```
src/main.rs           axum wiring, config from env, graceful shutdown, the
                      one HTTP-metrics middleware layer covering every route
src/state.rs          AppState (reqwest client + caches + Metrics), AppError
src/metrics.rs         Prometheus registry + counters/histograms, /metrics render
src/mcp_server.rs      InfoServer — the #[tool_router] exposing the 7 MCP tools,
                       each one line of `self.instrumented("name", ...)`
src/wordpress.rs       WP REST API client + typed post/page structs
src/resume.rs          Google Doc fetch + table-walk → Markdown conversion
src/html_convert.rs     shared HTML cleanup (strip hidden/decorative elements,
                        unwrap Google redirect links) + htmd Markdown conversion
src/resume_route.rs     plain GET /coopers-resume handler, reuses resume.rs
src/countdown.rs        countdown.cooperlees.com JSON API client + typed structs
```

Built with [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) (the
official Rust MCP SDK) via its `StreamableHttpService`, mounted into a
normal [`axum`](https://github.com/tokio-rs/axum) router alongside the plain
HTTP routes — one binary, one port, one `axum::serve` call.
