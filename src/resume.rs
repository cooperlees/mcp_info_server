use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use scraper::{Html, Selector};
use serde::Serialize;

use crate::html_convert::{
    html_to_markdown, strip_decorative_images, unwrap_google_redirect_links,
};
use crate::state::{AppError, AppState, unwrap_cache_error};

static TABLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("table").expect("valid selector"));
static ROW: LazyLock<Selector> = LazyLock::new(|| Selector::parse("tr").expect("valid selector"));
static CELL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("td").expect("valid selector"));

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ResumeDocument {
    pub markdown: String,
    pub fetched_at: DateTime<Utc>,
    pub source_url: String,
}

fn export_url(base: &str, doc_id: &str) -> String {
    format!("{base}/document/d/{doc_id}/export?format=html")
}

/// Convert the Google Doc's exported HTML into resume Markdown.
///
/// Expects a single `<table>` whose rows are 2-cell `label | content` pairs
/// (Google Docs' export shape for a resume laid out with section headings in
/// the left column). Falls back to converting the whole document body when
/// that shape isn't found, so a doc reformat degrades gracefully instead of
/// failing outright.
///
/// CPU-bound (DOM parse + multiple Markdown conversions) — always call this
/// through `tokio::task::spawn_blocking` from async code, never inline.
fn convert_resume_html(raw_html: &str) -> Result<String, AppError> {
    let mut document = Html::parse_document(raw_html);
    strip_decorative_images(&mut document);
    unwrap_google_redirect_links(&mut document);

    if let Some(sections) = convert_via_table(&document)?
        && !sections.is_empty()
    {
        return Ok(sections.join("\n\n"));
    }

    html_to_markdown(&document.root_element().inner_html())
}

fn convert_via_table(document: &Html) -> Result<Option<Vec<String>>, AppError> {
    let Some(table) = document.select(&TABLE).next() else {
        return Ok(None);
    };

    let mut sections = Vec::new();
    for row in table.select(&ROW) {
        let cells: Vec<_> = row.select(&CELL).collect();
        let [label_cell, content_cell] = cells.as_slice() else {
            continue;
        };

        let label = label_cell.text().collect::<String>().trim().to_owned();
        let content_md = html_to_markdown(&content_cell.inner_html())?;
        let content_md = content_md.trim();
        if content_md.is_empty() {
            continue;
        }

        if label.is_empty() {
            sections.push(content_md.to_owned());
        } else {
            sections.push(format!("## {label}\n\n{content_md}"));
        }
    }
    Ok(Some(sections))
}

/// Fetch and render the resume, backed by two caches: the raw HTML export is
/// cached for 45 minutes (`AppState::fetch_cached`), and the fully-rendered
/// `ResumeDocument` (including `fetched_at`) is cached again on top of that
/// for 1 minute, so bursts of calls from an MCP client (tool call + a human
/// hitting `/coopers-resume` moments later) share one render.
pub async fn get_resume(state: &AppState) -> Result<ResumeDocument, AppError> {
    let cache = state.resume_cache.clone();
    let metrics = state.metrics.clone();
    let state = state.clone();
    let entry = cache
        .entry(())
        .or_try_insert_with(async move {
            let source_url = export_url(state.resume_base_url(), &state.resume_doc_id);
            let raw_html = state.fetch_cached(&source_url).await?;
            let markdown = tokio::task::spawn_blocking(move || convert_resume_html(&raw_html))
                .await
                .map_err(|join_err| {
                    AppError::Other(format!("resume conversion task panicked: {join_err}"))
                })??;
            Ok::<ResumeDocument, AppError>(ResumeDocument {
                markdown,
                fetched_at: Utc::now(),
                source_url,
            })
        })
        .await
        .map_err(unwrap_cache_error)?;
    metrics.record_cache("resume", !entry.is_fresh());
    Ok(entry.into_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/resume_export.html");

    #[test]
    fn converts_known_sections_from_fixture() {
        let markdown = convert_resume_html(FIXTURE).unwrap();
        for heading in ["Skills", "Experience", "Education"] {
            assert!(
                markdown.contains(heading),
                "expected fixture markdown to contain a {heading:?} section, got:\n{markdown}"
            );
        }
    }

    #[test]
    fn unwraps_a_google_redirect_link_from_fixture() {
        let markdown = convert_resume_html(FIXTURE).unwrap();
        assert!(
            !markdown.contains("google.com/url"),
            "expected all Google redirect links to be unwrapped, got:\n{markdown}"
        );
    }

    #[test]
    fn strips_decorative_spacer_images() {
        let markdown = convert_resume_html(FIXTURE).unwrap();
        assert!(!markdown.contains("data:image"));
    }

    #[test]
    fn falls_back_to_whole_body_when_no_table_present() {
        let html = "<html><body><h1>Cooper Lees</h1><p>No table here.</p></body></html>";
        let markdown = convert_resume_html(html).unwrap();
        assert!(markdown.contains("Cooper Lees"));
        assert!(markdown.contains("No table here."));
    }

    #[test]
    fn falls_back_when_table_rows_have_wrong_cell_count() {
        let html = "<html><body><table><tr><td>only one cell</td></tr></table><p>Fallback text</p></body></html>";
        let markdown = convert_resume_html(html).unwrap();
        assert!(markdown.contains("Fallback text"));
    }

    #[test]
    fn two_cell_rows_become_headed_sections() {
        let html = "<table><tr><td>Skills</td><td><p>Rust, Ansible</p></td></tr></table>";
        let markdown = convert_resume_html(html).unwrap();
        assert_eq!(markdown, "## Skills\n\nRust, Ansible");
    }

    #[tokio::test]
    async fn get_resume_serves_repeated_calls_from_the_one_minute_cache() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/document/d/doc123/export")
            .match_query(mockito::Matcher::UrlEncoded("format".into(), "html".into()))
            .with_status(200)
            .with_body("<table><tr><td>Skills</td><td><p>Rust</p></td></tr></table>")
            .expect(1)
            .create_async()
            .await;

        let state = AppState::new(String::new(), "doc123".to_owned())
            .unwrap()
            .with_resume_base_url(server.url());

        let first = get_resume(&state).await.unwrap();
        let second = get_resume(&state).await.unwrap();

        // Same `fetched_at` proves the whole rendered document was served
        // from the resume-level cache, not just the underlying HTML fetch.
        assert_eq!(first.fetched_at, second.fetched_at);
        assert_eq!(first.markdown, "## Skills\n\nRust");
        mock.assert_async().await;
    }
}
