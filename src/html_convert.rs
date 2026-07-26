use std::sync::LazyLock;

use scraper::{Html, Node, Selector, StrTendril};

use crate::state::AppError;

static DECORATIVE_IMG: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("img").expect("valid selector"));
static GOOGLE_REDIRECT_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(r#"a[href^="https://www.google.com/url"]"#).expect("valid selector")
});
static STYLED: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("[style]").expect("valid selector"));

/// Convert an HTML fragment to Markdown, first dropping any `display: none`
/// elements. WordPress content is the motivating case: some posts on this
/// site embed a hidden div holding the raw, HTML-entity-escaped Markdown
/// source (a side effect of the site's Markdown-rendering plugin) right next
/// to the real, visible rendered content — without this, that hidden source
/// shows up verbatim (with escaped `<tags>`) at the top of every converted
/// post.
pub fn html_to_markdown(html: &str) -> Result<String, AppError> {
    let mut document = Html::parse_fragment(html);
    strip_hidden_elements(&mut document);
    Ok(htmd::convert(&document.root_element().inner_html())?)
}

/// Remove elements with an inline `display: none` style, in place.
pub fn strip_hidden_elements(document: &mut Html) {
    let ids: Vec<_> = document
        .select(&STYLED)
        .filter(|el| {
            el.value().attr("style").is_some_and(|style| {
                style
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
                    .to_lowercase()
                    .contains("display:none")
            })
        })
        .map(|el| el.id())
        .collect();

    for id in ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
}

/// Remove `<img>` tags whose `src` is a `data:` URI (decorative spacers), in place.
pub fn strip_decorative_images(document: &mut Html) {
    let ids: Vec<_> = document
        .select(&DECORATIVE_IMG)
        .filter(|el| {
            el.value()
                .attr("src")
                .is_some_and(|src| src.starts_with("data:"))
        })
        .map(|el| el.id())
        .collect();

    for id in ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
}

/// Rewrite Google's `/url?q=<real>&sa=...` redirect wrapper links to their real target, in place.
pub fn unwrap_google_redirect_links(document: &mut Html) {
    let ids: Vec<_> = document
        .select(&GOOGLE_REDIRECT_LINK)
        .map(|el| el.id())
        .collect();

    for id in ids {
        let Some(mut node) = document.tree.get_mut(id) else {
            continue;
        };
        let Node::Element(element) = node.value() else {
            continue;
        };
        let Some((_, href)) = element.attrs.iter_mut().find(|(k, _)| &*k.local == "href") else {
            continue;
        };
        let Ok(parsed) = url::Url::parse(href) else {
            continue;
        };
        let Some((_, real)) = parsed.query_pairs().find(|(k, _)| k == "q") else {
            continue;
        };
        *href = StrTendril::from(real.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_markdown_converts_basic_tags() {
        let md = html_to_markdown("<h2>Hi</h2><p>there</p>").unwrap();
        assert_eq!(md, "## Hi\n\nthere");
    }

    #[test]
    fn html_to_markdown_drops_hidden_source_div() {
        // Mirrors the real cooperlees.com markdown plugin's markup: a hidden
        // div holding the escaped raw source, alongside the real rendered
        // content.
        let html = concat!(
            r#"<div class="gfmr-markdown-container">"#,
            r#"<div class="gfmr-markdown-source" style="display: none;">&lt;h1&gt;Title&lt;/h1&gt;</div>"#,
            r#"<div class="gfmr-markdown-rendered"><h1>Title</h1><p>Real content.</p></div>"#,
            r#"</div>"#,
        );
        let md = html_to_markdown(html).unwrap();
        assert_eq!(md, "# Title\n\nReal content.");
        assert!(!md.contains(r"\<h1\>"));
    }

    #[test]
    fn strip_hidden_elements_ignores_visible_inline_styles() {
        let mut doc = Html::parse_fragment(r#"<p style="color: red;">Visible</p>"#);
        strip_hidden_elements(&mut doc);
        assert!(doc.root_element().inner_html().contains("Visible"));
    }

    #[test]
    fn strip_decorative_images_removes_data_uri_imgs_only() {
        let mut doc = Html::parse_fragment(
            r#"<p><img src="data:image/gif;base64,AAAA"><img src="https://example.com/real.png"></p>"#,
        );
        strip_decorative_images(&mut doc);
        let remaining = doc.root_element().inner_html();
        assert!(!remaining.contains("data:"));
        assert!(remaining.contains("https://example.com/real.png"));
    }

    #[test]
    fn unwrap_google_redirect_links_extracts_real_target() {
        let mut doc = Html::parse_fragment(
            r#"<a href="https://www.google.com/url?q=https://example.com/real&amp;sa=D&amp;ust=123">link</a>"#,
        );
        unwrap_google_redirect_links(&mut doc);
        let cleaned = doc.root_element().inner_html();
        assert!(cleaned.contains(r#"href="https://example.com/real""#));
        assert!(!cleaned.contains("google.com"));
    }

    #[test]
    fn unwrap_google_redirect_links_leaves_normal_links_untouched() {
        let mut doc = Html::parse_fragment(r#"<a href="https://example.com/plain">link</a>"#);
        unwrap_google_redirect_links(&mut doc);
        let cleaned = doc.root_element().inner_html();
        assert!(cleaned.contains(r#"href="https://example.com/plain""#));
    }
}
