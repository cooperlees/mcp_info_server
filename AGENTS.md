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
```

All four CI checks (fmt, clippy, test, release build) must pass locally before pushing — this
mirrors `.github/workflows/ci.yml` and `clippy.yml` exactly, so a clean local run means CI will pass.

## Keep the root-route ASCII banner in sync

`src/main.rs`'s `BANNER` const (served at `GET /`) lists every MCP tool and every plain HTTP route.
**Whenever you add, rename, or remove an MCP tool (in `src/mcp_server.rs`) or an HTTP route (in
`src/main.rs`'s router), update `BANNER` in the same change** so the two never drift apart. Keep the
existing tone: the terse "MCP" figlet header, a one-line Cooper-Lees-flavored intro (SRE, Rust, IPv6,
public/no-auth), then a plain `Routes:` list — no extra decorative ASCII beyond the header, it reads
better plain.

## Architecture

```
src/main.rs           axum wiring, config from env, graceful shutdown, / and /healthz handlers
src/state.rs           AppState (reqwest client + caches), AppError
src/mcp_server.rs       InfoServer — the #[tool_router] exposing the MCP tools
src/wordpress.rs        WP REST API client + typed post/page structs
src/resume.rs           Google Doc fetch + table-walk → Markdown conversion
src/html_convert.rs      shared HTML cleanup (strip hidden/decorative elements, unwrap Google
                        redirect links) + htmd Markdown conversion
src/resume_route.rs      plain GET /coopers-resume handler, reuses resume.rs
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
