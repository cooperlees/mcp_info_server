use serde::{Deserialize, Serialize};

use crate::html_convert::html_to_markdown;
use crate::state::{AppError, AppState};

#[derive(Debug, Clone, Deserialize)]
struct WpRendered {
    rendered: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WpItemRaw {
    id: u64,
    slug: String,
    link: String,
    date: String,
    title: WpRendered,
    excerpt: WpRendered,
    content: WpRendered,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ListRequest {
    /// 1-indexed page of results.
    #[serde(default)]
    pub page: Option<i32>,
    /// Results per page (WordPress default/max is 100).
    #[serde(default)]
    pub per_page: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SlugRequest {
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PostSummary {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub excerpt_markdown: String,
    pub link: String,
    pub date: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PostDetail {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub content_markdown: String,
    pub link: String,
    pub date: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PageSummary {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub excerpt_markdown: String,
    pub link: String,
    pub date: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PageDetail {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub content_markdown: String,
    pub link: String,
    pub date: String,
}

fn summary_from_raw(
    raw: WpItemRaw,
) -> Result<(i64, String, String, String, String, String), AppError> {
    let excerpt_markdown = html_to_markdown(&raw.excerpt.rendered)?;
    Ok((
        raw.id as i64,
        raw.slug,
        raw.title.rendered,
        excerpt_markdown,
        raw.link,
        raw.date,
    ))
}

impl TryFrom<WpItemRaw> for PostSummary {
    type Error = AppError;
    fn try_from(raw: WpItemRaw) -> Result<Self, AppError> {
        let (id, slug, title, excerpt_markdown, link, date) = summary_from_raw(raw)?;
        Ok(Self {
            id,
            slug,
            title,
            excerpt_markdown,
            link,
            date,
        })
    }
}

impl TryFrom<WpItemRaw> for PageSummary {
    type Error = AppError;
    fn try_from(raw: WpItemRaw) -> Result<Self, AppError> {
        let (id, slug, title, excerpt_markdown, link, date) = summary_from_raw(raw)?;
        Ok(Self {
            id,
            slug,
            title,
            excerpt_markdown,
            link,
            date,
        })
    }
}

impl TryFrom<WpItemRaw> for PostDetail {
    type Error = AppError;
    fn try_from(raw: WpItemRaw) -> Result<Self, AppError> {
        let content_markdown = html_to_markdown(&raw.content.rendered)?;
        Ok(Self {
            id: raw.id as i64,
            slug: raw.slug,
            title: raw.title.rendered,
            content_markdown,
            link: raw.link,
            date: raw.date,
        })
    }
}

impl TryFrom<WpItemRaw> for PageDetail {
    type Error = AppError;
    fn try_from(raw: WpItemRaw) -> Result<Self, AppError> {
        let content_markdown = html_to_markdown(&raw.content.rendered)?;
        Ok(Self {
            id: raw.id as i64,
            slug: raw.slug,
            title: raw.title.rendered,
            content_markdown,
            link: raw.link,
            date: raw.date,
        })
    }
}

fn list_url(base: &str, endpoint: &str, req: &ListRequest) -> String {
    let mut url = format!("{base}/wp-json/wp/v2/{endpoint}?");
    if let Some(page) = req.page {
        url.push_str(&format!("page={page}&"));
    }
    if let Some(per_page) = req.per_page {
        url.push_str(&format!("per_page={per_page}&"));
    }
    url
}

fn slug_url(base: &str, endpoint: &str, slug: &str) -> String {
    format!("{base}/wp-json/wp/v2/{endpoint}?slug={}", encode_slug(slug))
}

/// WP slugs are URL-safe by construction (lowercase, hyphenated); this just
/// guards against surprises rather than pulling in a percent-encoding crate.
fn encode_slug(slug: &str) -> String {
    slug.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

/// Runs the (CPU-bound, HTML-to-Markdown-converting) `TryFrom` conversion on
/// the blocking thread pool so a large batch of posts never stalls the async
/// runtime.
async fn convert_blocking<T, F>(convert: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    tokio::task::spawn_blocking(convert)
        .await
        .map_err(|join_err| AppError::Other(format!("conversion task panicked: {join_err}")))?
}

#[tracing::instrument(level = "debug", skip(state))]
async fn fetch_list<T>(
    state: &AppState,
    endpoint: &str,
    req: &ListRequest,
) -> Result<Vec<T>, AppError>
where
    T: TryFrom<WpItemRaw, Error = AppError> + Send + 'static,
{
    let url = list_url(&state.wordpress_url, endpoint, req);
    let body = state.fetch_cached(&url).await?;
    let raw: Vec<WpItemRaw> = serde_json::from_str(&body)?;
    let items: Vec<T> =
        convert_blocking(move || raw.into_iter().map(T::try_from).collect()).await?;
    tracing::debug!(count = items.len(), "fetched list");
    Ok(items)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn fetch_one<T>(state: &AppState, endpoint: &str, slug: &str) -> Result<T, AppError>
where
    T: TryFrom<WpItemRaw, Error = AppError> + Send + 'static,
{
    let url = slug_url(&state.wordpress_url, endpoint, slug);
    let body = state.fetch_cached(&url).await?;
    let mut raw: Vec<WpItemRaw> = serde_json::from_str(&body)?;
    let item = raw
        .pop()
        .ok_or_else(|| AppError::NotFound(format!("no {endpoint} found with slug {slug:?}")))?;
    convert_blocking(move || T::try_from(item)).await
}

pub async fn list_posts(state: &AppState, req: ListRequest) -> Result<Vec<PostSummary>, AppError> {
    fetch_list(state, "posts", &req).await
}

pub async fn get_post(state: &AppState, slug: &str) -> Result<PostDetail, AppError> {
    fetch_one(state, "posts", slug).await
}

pub async fn list_pages(state: &AppState, req: ListRequest) -> Result<Vec<PageSummary>, AppError> {
    fetch_list(state, "pages", &req).await
}

pub async fn get_page(state: &AppState, slug: &str) -> Result<PageDetail, AppError> {
    fetch_one(state, "pages", slug).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item_json(id: u64, slug: &str) -> String {
        format!(
            r#"{{
                "id": {id},
                "slug": "{slug}",
                "link": "https://cooperlees.com/{slug}/",
                "date": "2026-01-01T00:00:00",
                "title": {{"rendered": "A Title"}},
                "excerpt": {{"rendered": "<p>An excerpt</p>\n"}},
                "content": {{"rendered": "<p>Full <strong>content</strong>.</p>\n"}},
                "yoast_head": "<meta ignored>",
                "_links": {{"self": [{{"href": "https://cooperlees.com/wp-json/wp/v2/posts/{id}"}}]}}
            }}"#
        )
    }

    #[test]
    fn deserializes_wp_item_ignoring_unknown_fields() {
        let json = sample_item_json(1, "hello-world");
        let raw: WpItemRaw = serde_json::from_str(&json).unwrap();
        assert_eq!(raw.id, 1);
        assert_eq!(raw.slug, "hello-world");
    }

    #[test]
    fn post_summary_converts_excerpt_to_markdown() {
        let raw: WpItemRaw = serde_json::from_str(&sample_item_json(1, "hello-world")).unwrap();
        let summary = PostSummary::try_from(raw).unwrap();
        assert_eq!(summary.excerpt_markdown, "An excerpt");
        assert_eq!(summary.title, "A Title");
    }

    #[test]
    fn post_detail_converts_content_to_markdown() {
        let raw: WpItemRaw = serde_json::from_str(&sample_item_json(1, "hello-world")).unwrap();
        let detail = PostDetail::try_from(raw).unwrap();
        assert_eq!(detail.content_markdown, "Full **content**.");
    }

    #[test]
    fn list_url_encodes_pagination_params() {
        let url = list_url(
            "https://cooperlees.com",
            "posts",
            &ListRequest {
                page: Some(2),
                per_page: Some(10),
            },
        );
        assert_eq!(
            url,
            "https://cooperlees.com/wp-json/wp/v2/posts?page=2&per_page=10&"
        );
    }

    #[test]
    fn slug_url_builds_expected_query() {
        let url = slug_url("https://cooperlees.com", "pages", "about-me");
        assert_eq!(
            url,
            "https://cooperlees.com/wp-json/wp/v2/pages?slug=about-me"
        );
    }

    #[tokio::test]
    async fn get_post_returns_not_found_for_empty_result_array() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/wp-json/wp/v2/posts?slug=missing")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;
        let state = AppState::new(
            server.url(),
            "unused".to_owned(),
            "unused".to_owned(),
            "unused".to_owned(),
            crate::rate_limit::RateLimitConfig::permissive(),
        )
        .unwrap();

        let err = get_post(&state, "missing").await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_post_returns_matching_item() {
        let mut server = mockito::Server::new_async().await;
        let body = format!("[{}]", sample_item_json(42, "found-me"));
        server
            .mock("GET", "/wp-json/wp/v2/posts?slug=found-me")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;
        let state = AppState::new(
            server.url(),
            "unused".to_owned(),
            "unused".to_owned(),
            "unused".to_owned(),
            crate::rate_limit::RateLimitConfig::permissive(),
        )
        .unwrap();

        let post = get_post(&state, "found-me").await.unwrap();
        assert_eq!(post.id, 42);
        assert_eq!(post.slug, "found-me");
    }

    #[tokio::test]
    async fn list_posts_parses_multiple_items() {
        let mut server = mockito::Server::new_async().await;
        let body = format!(
            "[{},{}]",
            sample_item_json(1, "one"),
            sample_item_json(2, "two")
        );
        server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/wp-json/wp/v2/posts".into()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;
        let state = AppState::new(
            server.url(),
            "unused".to_owned(),
            "unused".to_owned(),
            "unused".to_owned(),
            crate::rate_limit::RateLimitConfig::permissive(),
        )
        .unwrap();

        let posts = list_posts(
            &state,
            ListRequest {
                page: None,
                per_page: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].slug, "one");
        assert_eq!(posts[1].slug, "two");
    }
}
