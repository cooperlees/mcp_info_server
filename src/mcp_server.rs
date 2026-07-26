use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;

use crate::resume::{self, ResumeDocument};
use crate::state::AppState;
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
}

#[tool_router(router = tool_router)]
impl InfoServer {
    #[tool(description = "List cooperlees.com blog posts, newest tool-default order, paginated.")]
    async fn list_posts(
        &self,
        Parameters(req): Parameters<ListRequest>,
    ) -> Result<Json<PostListResult>, String> {
        wordpress::list_posts(&self.state, req)
            .await
            .map(|posts| Json(PostListResult { posts }))
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Get a single cooperlees.com blog post by slug, with full content as Markdown."
    )]
    async fn get_post(
        &self,
        Parameters(SlugRequest { slug }): Parameters<SlugRequest>,
    ) -> Result<Json<PostDetail>, String> {
        wordpress::get_post(&self.state, &slug)
            .await
            .map(Json)
            .map_err(|e| e.to_string())
    }

    #[tool(description = "List cooperlees.com static pages, paginated.")]
    async fn list_pages(
        &self,
        Parameters(req): Parameters<ListRequest>,
    ) -> Result<Json<PageListResult>, String> {
        wordpress::list_pages(&self.state, req)
            .await
            .map(|pages| Json(PageListResult { pages }))
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Get a single cooperlees.com static page by slug, with full content as Markdown."
    )]
    async fn get_page(
        &self,
        Parameters(SlugRequest { slug }): Parameters<SlugRequest>,
    ) -> Result<Json<PageDetail>, String> {
        wordpress::get_page(&self.state, &slug)
            .await
            .map(Json)
            .map_err(|e| e.to_string())
    }

    #[tool(description = "Get Cooper Lees' resume, rendered as Markdown.")]
    async fn get_resume(&self) -> Result<Json<ResumeDocument>, String> {
        resume::get_resume(&self.state)
            .await
            .map(Json)
            .map_err(|e| e.to_string())
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
}
