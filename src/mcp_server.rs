use std::future::Future;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;
use tracing::Instrument;

use crate::countdown::{self, Countdown, CountdownList};
use crate::resume::{self, ResumeDocument};
use crate::state::{AppError, AppState};
use crate::wordpress::{
    self, ListRequest, PageDetail, PageSummary, PostDetail, PostSummary, SlugRequest,
};

// The MCP spec requires a tool's output schema to be rooted at an `object`,
// not an `array` — a bare `Vec<T>` gets rejected, so list tools wrap their
// results in one of these instead.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct PostListResult {
    posts: Vec<PostSummary>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct PageListResult {
    pages: Vec<PageSummary>,
}

#[derive(Clone)]
pub struct InfoServer {
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl InfoServer {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    /// Times `fut`, records `mcp_info_server_tool_call_duration_seconds` and
    /// `mcp_info_server_tool_calls_total{tool, result}` for it, and — on
    /// failure — `mcp_info_server_errors_total` too. Every tool method is
    /// just one call to this, so none of them repeat the instrumentation.
    /// Also opens a `tool_call` span (visible with `RUST_LOG=debug`) so any
    /// spans/logs `fut` produces (cache hits, upstream fetches) nest under
    /// it with the tool name for free.
    async fn instrumented<T, F>(&self, tool: &str, fut: F) -> Result<Json<T>, String>
    where
        F: Future<Output = Result<T, AppError>>,
    {
        let span = tracing::debug_span!("tool_call", tool);
        async move {
            let timer = self
                .state
                .metrics
                .tool_call_duration_seconds
                .with_label_values(&[tool])
                .start_timer();
            let result = fut.await;
            timer.observe_duration();

            match result {
                Ok(value) => {
                    self.state
                        .metrics
                        .tool_calls_total
                        .with_label_values(&[tool, "ok"])
                        .inc();
                    Ok(Json(value))
                }
                Err(err) => {
                    self.state
                        .metrics
                        .tool_calls_total
                        .with_label_values(&[tool, "error"])
                        .inc();
                    self.state.metrics.record_error(&err);
                    // NotFound is a normal client outcome (bad slug); anything
                    // else is an actual upstream/system failure worth a warn.
                    if matches!(err, AppError::NotFound(_)) {
                        tracing::debug!(error = %err, "tool call not found");
                    } else {
                        tracing::warn!(error = %err, kind = err.kind(), "tool call failed");
                    }
                    Err(err.to_string())
                }
            }
        }
        .instrument(span)
        .await
    }
}

#[tool_router(router = tool_router)]
impl InfoServer {
    #[tool(description = "List cooperlees.com blog posts, newest tool-default order, paginated.")]
    async fn list_posts(
        &self,
        Parameters(req): Parameters<ListRequest>,
    ) -> Result<Json<PostListResult>, String> {
        self.instrumented("list_posts", async {
            wordpress::list_posts(&self.state, req)
                .await
                .map(|posts| PostListResult { posts })
        })
        .await
    }

    #[tool(
        description = "Get a single cooperlees.com blog post by slug, with full content as Markdown."
    )]
    async fn get_post(
        &self,
        Parameters(SlugRequest { slug }): Parameters<SlugRequest>,
    ) -> Result<Json<PostDetail>, String> {
        self.instrumented("get_post", wordpress::get_post(&self.state, &slug))
            .await
    }

    #[tool(description = "List cooperlees.com static pages, paginated.")]
    async fn list_pages(
        &self,
        Parameters(req): Parameters<ListRequest>,
    ) -> Result<Json<PageListResult>, String> {
        self.instrumented("list_pages", async {
            wordpress::list_pages(&self.state, req)
                .await
                .map(|pages| PageListResult { pages })
        })
        .await
    }

    #[tool(
        description = "Get a single cooperlees.com static page by slug, with full content as Markdown."
    )]
    async fn get_page(
        &self,
        Parameters(SlugRequest { slug }): Parameters<SlugRequest>,
    ) -> Result<Json<PageDetail>, String> {
        self.instrumented("get_page", wordpress::get_page(&self.state, &slug))
            .await
    }

    #[tool(description = "Get Cooper Lees' resume, rendered as Markdown.")]
    async fn get_resume(&self) -> Result<Json<ResumeDocument>, String> {
        self.instrumented("get_resume", resume::get_resume(&self.state))
            .await
    }

    #[tool(
        description = "List all of Cooper Lees' countdown.cooperlees.com events (trips, birthdays, \
                        weddings, etc), soonest first, each with live time-remaining."
    )]
    async fn list_countdowns(&self) -> Result<Json<CountdownList>, String> {
        self.instrumented("list_countdowns", countdown::list_countdowns(&self.state))
            .await
    }

    #[tool(
        description = "Get a single countdown.cooperlees.com event by slug (see list_countdowns), \
                        with live time-remaining."
    )]
    async fn get_countdown(
        &self,
        Parameters(SlugRequest { slug }): Parameters<SlugRequest>,
    ) -> Result<Json<Countdown>, String> {
        self.instrumented(
            "get_countdown",
            countdown::get_countdown(&self.state, &slug),
        )
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for InfoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Read-only access to cooperlees.com's blog posts, pages, and Cooper Lees' resume. \
             All content served here is already public.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server() -> InfoServer {
        InfoServer::new(
            AppState::new(
                "https://example.invalid".to_owned(),
                "doc-id".to_owned(),
                "https://example.invalid".to_owned(),
                "https://example.invalid".to_owned(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn exposes_all_seven_tools() {
        let server = test_server();
        let names: Vec<_> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for expected in [
            "list_posts",
            "get_post",
            "list_pages",
            "get_page",
            "get_resume",
            "list_countdowns",
            "get_countdown",
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "missing tool {expected:?} in {names:?}"
            );
        }
    }

    // schemars (see json_schema_impls::primitives in the vendored source)
    // maps every Rust integer type except i32/i64 to a "format" string
    // outside any standard vocabulary: uint8/16/32/64/128/uint for
    // u8/u16/u32/u64/u128/usize, int8/16/128/int for i8/i16/i128/isize. MCP
    // clients' JSON Schema validators don't recognize them and log "unknown
    // format ... ignored" noise on every tool call - see the wordpress::id /
    // ListRequest::page fixes. This is the full, closed set schemars can
    // ever produce, so tool schema fields must stick to i32/i64 for whole
    // numbers.
    const UNRECOGNIZED_INTEGER_FORMATS: &[&str] = &[
        "uint8", "uint16", "uint32", "uint64", "uint128", "uint", "int8", "int16", "int128", "int",
    ];

    fn collect_unrecognized_formats(
        path: String,
        value: &serde_json::Value,
        out: &mut Vec<String>,
    ) {
        let serde_json::Value::Object(map) = value else {
            if let serde_json::Value::Array(items) = value {
                for (i, item) in items.iter().enumerate() {
                    collect_unrecognized_formats(format!("{path}[{i}]"), item, out);
                }
            }
            return;
        };
        if let Some(serde_json::Value::String(format)) = map.get("format")
            && UNRECOGNIZED_INTEGER_FORMATS.contains(&format.as_str())
        {
            out.push(format!("{path}: format={format:?}"));
        }
        for (key, child) in map {
            collect_unrecognized_formats(format!("{path}/{key}"), child, out);
        }
    }

    #[test]
    fn tool_schemas_avoid_integer_types_schemars_cant_format_for_clients() {
        let server = test_server();
        let mut unrecognized = Vec::new();
        for tool in server.tool_router.list_all() {
            collect_unrecognized_formats(
                format!("{}#input_schema", tool.name),
                &serde_json::Value::Object((*tool.input_schema).clone()),
                &mut unrecognized,
            );
            if let Some(output_schema) = &tool.output_schema {
                collect_unrecognized_formats(
                    format!("{}#output_schema", tool.name),
                    &serde_json::Value::Object((**output_schema).clone()),
                    &mut unrecognized,
                );
            }
        }
        assert!(
            unrecognized.is_empty(),
            "tool schemas use a Rust integer type schemars can't format for MCP clients \
             (likely a u8/u16/u32/u64/u128/usize or i8/i16/i128/isize field - use i32/i64 \
             instead): {unrecognized:#?}"
        );
    }

    #[tokio::test]
    async fn instrumented_records_ok_result_and_duration() {
        let server = test_server();
        let result: Result<Json<u32>, String> =
            server.instrumented("dummy_tool", async { Ok(7u32) }).await;
        assert_eq!(result.unwrap().0, 7);

        let body = server.state.metrics.render().unwrap();
        assert!(
            body.contains(r#"mcp_info_server_tool_calls_total{result="ok",tool="dummy_tool"} 1"#)
        );
        assert!(
            body.contains(
                "mcp_info_server_tool_call_duration_seconds_count{tool=\"dummy_tool\"} 1"
            )
        );
    }

    #[tokio::test]
    async fn instrumented_records_error_result_and_error_kind() {
        let server = test_server();
        let result: Result<Json<u32>, String> = server
            .instrumented("dummy_tool", async {
                Err(AppError::NotFound("x".to_owned()))
            })
            .await;
        assert!(result.is_err());

        let body = server.state.metrics.render().unwrap();
        assert!(
            body.contains(
                r#"mcp_info_server_tool_calls_total{result="error",tool="dummy_tool"} 1"#
            )
        );
        assert!(body.contains(r#"mcp_info_server_errors_total{kind="not_found"} 1"#));
    }
}
