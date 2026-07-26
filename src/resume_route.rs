use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

use crate::resume;
use crate::state::AppState;

/// Plain `GET /coopers-resume` — the same cached, rendered resume the
/// `get_resume` MCP tool serves, reachable as an ordinary URL (`curl`, a
/// shared link) with no MCP client required.
pub async fn coopers_resume(State(state): State<AppState>) -> impl IntoResponse {
    match resume::get_resume(&state).await {
        Ok(doc) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            doc.markdown,
        )
            .into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn returns_markdown_content_type_and_body_on_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/document/d/doc123/export")
            .match_query(mockito::Matcher::UrlEncoded("format".into(), "html".into()))
            .with_status(200)
            .with_body("<table><tr><td>Skills</td><td><p>Rust</p></td></tr></table>")
            .create_async()
            .await;
        let state = AppState::new(String::new(), "doc123".to_owned())
            .unwrap()
            .with_resume_base_url(server.url());

        let app = Router::new()
            .route("/coopers-resume", get(coopers_resume))
            .with_state(state);

        let response = app
            .oneshot(Request::get("/coopers-resume").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/markdown; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"## Skills\n\nRust");
    }

    #[tokio::test]
    async fn returns_bad_gateway_when_upstream_fails() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/document/d/doc123/export")
            .match_query(mockito::Matcher::UrlEncoded("format".into(), "html".into()))
            .with_status(500)
            .create_async()
            .await;
        let state = AppState::new(String::new(), "doc123".to_owned())
            .unwrap()
            .with_resume_base_url(server.url());

        let app = Router::new()
            .route("/coopers-resume", get(coopers_resume))
            .with_state(state);

        let response = app
            .oneshot(Request::get("/coopers-resume").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
}
