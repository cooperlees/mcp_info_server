use std::future::Future;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;

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
    async fn instrumented<T, F>(&self, tool: &str, fut: F) -> Result<Json<T>, String>
    where
        F: Future<Output = Result<T, AppError>>,
    {
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
                Err(err.to_string())
            }
        }
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
            AppState::new("https://example.invalid".to_owned(), "doc-id".to_owned()).unwrap(),
        )
    }

    #[test]
    fn exposes_all_five_tools() {
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
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "missing tool {expected:?} in {names:?}"
            );
        }
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
