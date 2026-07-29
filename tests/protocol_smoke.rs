//! Wire-level regression gate for the MCP protocol handshake and this
//! server's security posture — the class of bug unit tests can't catch,
//! since they exercise Rust-internal logic, not the actual JSON-RPC/HTTP
//! contract a client sees over the wire (this file exists because exactly
//! that gap let `serverInfo` leak the SDK's identity instead of this
//! server's, past `cargo test`, unnoticed until a live curl session).
//!
//! Run before shipping a release:
//!   cargo test --test protocol_smoke
//! (spawns the just-built binary locally on a free port — no config needed)
//!
//! Run after deploying one, to check the live server instead of a local
//! build:
//!   MCP_SMOKE_TEST_URL=https://mcp.cooperlees.com cargo test --test protocol_smoke
//!
//! Both modes run the exact same checks against whichever base URL results.

use std::collections::HashSet;
use std::net::TcpListener;
use std::process::{Child, Command};
use std::time::Duration;

use serde_json::{Value, json};

const EXPECTED_TOOLS: &[&str] = &[
    "list_posts",
    "get_post",
    "list_pages",
    "get_page",
    "get_resume",
    "list_countdowns",
    "get_countdown",
];

/// Kills the spawned server on drop, so a panicking assertion mid-test
/// doesn't leak an orphaned process across repeated `cargo test` runs.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Either the caller-provided `MCP_SMOKE_TEST_URL` (post-release: point this
/// at a live deployment), or a freshly spawned copy of the just-built binary
/// on a free local port (pre-release: exactly what you're about to ship).
/// The guard, when present, must be held for as long as `base_url` is used.
async fn target() -> (String, Option<ChildGuard>) {
    if let Ok(url) = std::env::var("MCP_SMOKE_TEST_URL") {
        return (url.trim_end_matches('/').to_owned(), None);
    }

    let port = TcpListener::bind("127.0.0.1:0")
        .expect("failed to reserve a free port")
        .local_addr()
        .expect("local_addr")
        .port();

    let child = Command::new(env!("CARGO_BIN_EXE_mcp_info_server"))
        .env("LISTEN_PORT", port.to_string())
        .env("RUST_LOG", "error")
        .spawn()
        .expect("failed to start mcp_info_server");
    let guard = ChildGuard(child);

    let base_url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .get(format!("{base_url}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("mcp_info_server didn't become healthy within 10s of starting");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    (base_url, Some(guard))
}

/// The initialize/tools-list response's `data:` line is the one JSON-RPC
/// message that matters here — the rest of the SSE framing (event ids,
/// retry hints) is transport plumbing this test doesn't care about.
fn parse_sse_json_rpc(body: &str) -> Value {
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                return v;
            }
        }
    }
    panic!("no JSON-RPC message found in SSE body:\n{body}");
}

/// Performs the initialize handshake and returns (parsed JSON-RPC response,
/// the `Mcp-Session-Id` the server assigned).
async fn initialize(
    client: &reqwest::Client,
    base_url: &str,
    protocol_version: &str,
) -> (Value, String) {
    let resp = client
        .post(format!("{base_url}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": "protocol_smoke", "version": "0.0.0"}
            }
        }))
        .send()
        .await
        .expect("initialize request failed");

    assert!(
        resp.status().is_success(),
        "initialize returned {}",
        resp.status()
    );
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .expect("initialize response missing Mcp-Session-Id header")
        .to_str()
        .expect("Mcp-Session-Id not valid ASCII")
        .to_owned();
    let body = resp.text().await.expect("reading initialize body");
    (parse_sse_json_rpc(&body), session_id)
}

#[tokio::test]
async fn handshake_reports_this_servers_own_identity_and_declared_capabilities() {
    let (base_url, _guard) = target().await;
    let client = reqwest::Client::new();

    let (response, _session_id) = initialize(&client, &base_url, "2025-06-18").await;
    let result = &response["result"];

    // Regression check for the "serverInfo: rmcp/2.2.0" bug: this must be
    // this server's own crate name/version, never the SDK's.
    assert_eq!(
        result["serverInfo"]["name"], "mcp_info_server",
        "serverInfo.name leaked the SDK's identity instead of this server's - full response: {result}"
    );
    assert_eq!(
        result["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "serverInfo.version doesn't match this build - full response: {result}"
    );

    assert_eq!(
        result["protocolVersion"], "2025-06-18",
        "server should echo back a protocol version it was asked for and supports"
    );

    // Only `tools` is implemented - resources/prompts/sampling/etc aren't,
    // and shouldn't be advertised as if they were.
    let capabilities = result["capabilities"]
        .as_object()
        .expect("capabilities should be an object");
    assert!(
        capabilities.contains_key("tools"),
        "capabilities missing tools: {capabilities:?}"
    );
    for unsupported in ["resources", "prompts", "sampling", "logging"] {
        assert!(
            !capabilities.contains_key(unsupported),
            "capabilities falsely advertises unimplemented '{unsupported}': {capabilities:?}"
        );
    }
}

#[tokio::test]
async fn tools_list_exposes_exactly_the_seven_documented_tools_with_valid_schemas() {
    let (base_url, _guard) = target().await;
    let client = reqwest::Client::new();
    let (_init, session_id) = initialize(&client, &base_url, "2025-06-18").await;

    let resp = client
        .post(format!("{base_url}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .send()
        .await
        .expect("tools/list request failed");
    assert!(
        resp.status().is_success(),
        "tools/list returned {}",
        resp.status()
    );
    let body = resp.text().await.expect("reading tools/list body");
    let response = parse_sse_json_rpc(&body);

    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools should be an array: {response}"));

    let names: HashSet<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool missing name"))
        .collect();
    let expected: HashSet<&str> = EXPECTED_TOOLS.iter().copied().collect();
    assert_eq!(
        names, expected,
        "tool set changed - update EXPECTED_TOOLS here (and BANNER in src/main.rs, per AGENTS.md) if intentional"
    );

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            !tool["description"].as_str().unwrap_or("").is_empty(),
            "{name} has no description"
        );
        assert!(
            tool.get("inputSchema").is_some(),
            "{name} missing inputSchema"
        );
        assert!(
            tool.get("outputSchema").is_some(),
            "{name} missing outputSchema - structured tool output regressed"
        );
    }
}

#[tokio::test]
async fn requests_from_an_unrecognized_host_are_rejected() {
    let (base_url, _guard) = target().await;
    let client = reqwest::Client::new();

    // Origin/Host validation is this transport's one hard MUST (DNS
    // rebinding protection, ALLOWED_HOSTS in this server) - a spoofed Host
    // must never reach a tool. Against the local spawn this hits the app's
    // own check directly (403); against a live URL behind Traefik, Traefik's
    // own Host-based router may reject it first instead (404) - either way
    // is fine, the invariant is just "rejected before it can do anything".
    let resp = client
        .post(format!("{base_url}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Host", "evil.example.com")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "protocol_smoke", "version": "0.0.0"}
            }
        }))
        .send()
        .await
        .expect("request failed");

    assert!(
        resp.status().is_client_error(),
        "a request with an unrecognized Host must be rejected (DNS-rebinding protection) - got {}",
        resp.status()
    );
}

#[tokio::test]
async fn requests_after_initialize_are_rejected_without_a_session_id() {
    let (base_url, _guard) = target().await;
    let client = reqwest::Client::new();
    initialize(&client, &base_url, "2025-06-18").await;

    let resp = client
        .post(format!("{base_url}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .send()
        .await
        .expect("request failed");

    assert!(
        resp.status().is_client_error(),
        "a request missing Mcp-Session-Id after initialize should be rejected, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn plain_http_routes_still_respond() {
    // Deliberately excludes /coopers-resume and /resume - those hit the real
    // Google Docs export URL, and this test has to stay hermetic (no live
    // third-party network calls) to run as part of ordinary `cargo test`.
    let (base_url, _guard) = target().await;
    let client = reqwest::Client::new();

    for path in ["/", "/healthz", "/metrics"] {
        let resp = client
            .get(format!("{base_url}{path}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {path} failed: {e}"));
        assert!(
            resp.status().is_success(),
            "GET {path} returned {}",
            resp.status()
        );
    }
}
