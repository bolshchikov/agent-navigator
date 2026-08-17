use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use url::Url;

static ARTICLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("article").expect("article"));
static MAIN: LazyLock<Selector> = LazyLock::new(|| Selector::parse("main").expect("main"));
static ROLE_MAIN: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("[role='main']").expect("role main"));
static BODY: LazyLock<Selector> = LazyLock::new(|| Selector::parse("body").expect("body"));
static TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").expect("title"));
static CANONICAL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("link[rel='canonical']").expect("canonical"));
static META: LazyLock<Selector> = LazyLock::new(|| Selector::parse("meta").expect("meta"));
static HTML: LazyLock<Selector> = LazyLock::new(|| Selector::parse("html").expect("html"));
static H1: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h1").expect("h1"));

const STRIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "iframe", "svg", "template", "nav", "footer", "aside",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractedPage {
    pub title: Option<String>,
    pub canonical_url: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub last_modified: Option<String>,
    pub markdown: String,
    pub visible_text: String,
    pub visible_text_len: usize,
    pub word_count: usize,
    pub script_count: usize,
    pub spa_shell: Option<String>,
    pub enable_js_hint: bool,
}

pub fn parse_html(html: &str) -> Html {
    Html::parse_document(html)
}

pub fn extract_page(html: &str, base_url: &str) -> ExtractedPage {
    let document = Html::parse_document(html);
    extract_from_document(&document, base_url)
}

pub fn extract_from_document(document: &Html, base_url: &str) -> ExtractedPage {
    let title = document
        .select(&TITLE)
        .next()
        .map(|el| collapse_ws(&el.text().collect::<String>()))
        .filter(|s| !s.is_empty());

    let canonical_url = document
        .select(&CANONICAL)
        .next()
        .and_then(|el| el.value().attr("href"))
        .and_then(|href| resolve_url(base_url, href));

    let mut description = None;
    let mut last_modified = None;
    for meta in document.select(&META) {
        let content = meta.value().attr("content").unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let name = meta
            .value()
            .attr("name")
            .or_else(|| meta.value().attr("property"))
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "description" | "og:description" | "twitter:description" if description.is_none() => {
                description = Some(content.to_string());
            }
            "last-modified" | "article:modified_time" | "og:updated_time"
                if last_modified.is_none() =>
            {
                last_modified = Some(content.to_string());
            }
            _ => {}
        }
    }

    let language = document
        .select(&HTML)
        .next()
        .and_then(|el| el.value().attr("lang"))
        .map(|s| s.to_string());

    let script_count = count_scripts(document);
    let spa_shell = detect_spa_shell(document);
    let enable_js_hint = detect_enable_js(document);

    let fragment_html = choose_content_html(document);
    let markdown = html_to_markdown(&fragment_html);
    let visible_text = visible_text_from_document(document);
    let word_count = word_count(&visible_text);

    let title = title.or_else(|| {
        document
            .select(&H1)
            .next()
            .map(|el| collapse_ws(&el.text().collect::<String>()))
            .filter(|s| !s.is_empty())
    });

    ExtractedPage {
        title,
        canonical_url,
        description,
        language,
        last_modified,
        markdown,
        visible_text_len: visible_text.chars().count(),
        word_count,
        visible_text,
        script_count,
        spa_shell,
        enable_js_hint,
    }
}

pub fn html_to_markdown(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(STRIP_TAGS.to_vec())
        .build();
    match converter.convert(html) {
        Ok(md) => collapse_blank_lines(&md),
        Err(_) => collapse_ws(
            &scraper::Html::parse_fragment(html)
                .root_element()
                .text()
                .collect::<String>(),
        ),
    }
}

fn choose_content_html(document: &Html) -> String {
    for sel in [&*ARTICLE, &*MAIN, &*ROLE_MAIN] {
        if let Some(el) = document.select(sel).next() {
            let text = collect_visible_text(el);
            if word_count(&text) >= 20 {
                return el.html();
            }
        }
    }
    if let Some(best) = best_block(document) {
        return best;
    }
    document
        .select(&BODY)
        .next()
        .map(|el| el.html())
        .unwrap_or_else(|| document.html())
}

fn best_block(document: &Html) -> Option<String> {
    let mut best: Option<(f64, String)> = None;
    let candidates = Selector::parse("article, main, section, div").ok()?;
    for el in document.select(&candidates) {
        let name = el.value().name();
        if STRIP_TAGS.contains(&name) {
            continue;
        }
        if is_unlikely(el) {
            continue;
        }
        let text = collect_visible_text(el);
        let words = word_count(&text) as f64;
        if words < 30.0 {
            continue;
        }
        let links = link_density(el, words);
        let score = words * (1.0 - links);
        match &best {
            Some((s, _)) if *s >= score => {}
            _ => best = Some((score, el.html())),
        }
    }
    best.map(|(_, html)| html)
}

fn is_unlikely(el: ElementRef<'_>) -> bool {
    let class_id = format!(
        "{} {}",
        el.value().attr("class").unwrap_or(""),
        el.value().attr("id").unwrap_or("")
    )
    .to_ascii_lowercase();
    const NEG: &[&str] = &[
        "nav",
        "footer",
        "header",
        "sidebar",
        "comment",
        "advert",
        "cookie",
        "modal",
        "popup",
        "share",
        "social",
        "related",
        "newsletter",
        "breadcrumb",
        "menu",
    ];
    NEG.iter().any(|n| class_id.contains(n))
}

fn link_density(el: ElementRef<'_>, words: f64) -> f64 {
    if words <= 0.0 {
        return 1.0;
    }
    let Ok(sel) = Selector::parse("a") else {
        return 0.0;
    };
    let link_words: usize = el
        .select(&sel)
        .map(|a| word_count(&a.text().collect::<String>()))
        .sum();
    ((link_words as f64) / words).clamp(0.0, 1.0)
}

fn count_scripts(document: &Html) -> usize {
    Selector::parse("script")
        .ok()
        .map(|sel| document.select(&sel).count())
        .unwrap_or(0)
}

fn detect_spa_shell(document: &Html) -> Option<String> {
    const IDS: &[&str] = &["root", "app", "__next", "__nuxt", "q-app", "svelte"];
    for id in IDS {
        let Ok(sel) = Selector::parse(&format!("#{id}")) else {
            continue;
        };
        if let Some(el) = document.select(&sel).next() {
            let words = word_count(&collect_visible_text(el));
            if words < 20 {
                return Some(format!(
                    "#{id} present with negligible text ({words} words)"
                ));
            }
        }
    }
    None
}

fn detect_enable_js(document: &Html) -> bool {
    let text = visible_text_from_document(document).to_ascii_lowercase();
    const HINTS: &[&str] = &[
        "enable javascript",
        "enable js",
        "requires javascript",
        "javascript is required",
        "turn on javascript",
        "you need javascript",
        "please enable javascript",
    ];
    HINTS.iter().any(|h| text.contains(h))
}

pub fn visible_text_from_document(document: &Html) -> String {
    document
        .select(&BODY)
        .next()
        .map(collect_visible_text)
        .unwrap_or_default()
}

pub fn collect_visible_text(el: ElementRef<'_>) -> String {
    let mut buf = String::new();
    walk_text(el, &mut buf);
    collapse_ws(&buf)
}

fn walk_text(el: ElementRef<'_>, buf: &mut String) {
    let name = el.value().name();
    if STRIP_TAGS.contains(&name) {
        return;
    }
    for child in el.children() {
        if let Some(text) = child.value().as_text() {
            buf.push_str(text);
            buf.push(' ');
        } else if let Some(child_el) = ElementRef::wrap(child) {
            walk_text(child_el, buf);
        }
    }
}

pub fn word_count(text: &str) -> usize {
    text.split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .count()
}

fn resolve_url(base_url: &str, href: &str) -> Option<String> {
    if let Ok(base) = Url::parse(base_url) {
        return base.join(href).ok().map(|u| u.to_string());
    }
    if href.starts_with("https://") || href.starts_with("http://") {
        return Some(href.to_string());
    }
    None
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank <= 2 {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tables_lists_and_links() {
        let html = r#"<html><body>
          <article>
            <h1>Guide</h1>
            <p>See the <a href="https://example.com/docs">docs</a>.</p>
            <ul><li>Alpha</li><li>Beta</li></ul>
            <table>
              <tr><th>Name</th><th>Value</th></tr>
              <tr><td>timeout</td><td>15s</td></tr>
            </table>
          </article>
        </body></html>"#;
        let page = extract_page(html, "https://example.com");
        assert!(page.markdown.contains("Guide"), "{}", page.markdown);
        assert!(
            page.markdown.contains("docs") && page.markdown.contains("https://example.com/docs"),
            "{}",
            page.markdown
        );
        assert!(page.markdown.contains("Alpha"), "{}", page.markdown);
        assert!(
            page.markdown.contains("timeout") && page.markdown.contains("15s"),
            "{}",
            page.markdown
        );
    }

    #[test]
    fn strips_nav_and_scripts() {
        let html = r#"<html><body>
          <nav>Home About Careers</nav>
          <script>window.__STATE = {huge: true}</script>
          <article><p>The actual article content lives here and is long enough to be selected as the main block of the page for agents reading static HTML.</p></article>
          <footer>Copyright 2099</footer>
        </body></html>"#;
        let page = extract_page(html, "https://example.com");
        assert!(
            page.markdown.contains("actual article content"),
            "{}",
            page.markdown
        );
        assert!(!page.markdown.contains("Careers"), "{}", page.markdown);
        assert!(!page.markdown.contains("__STATE"), "{}", page.markdown);
    }

    #[test]
    fn resolves_relative_canonical() {
        let html = r#"<html><head><link rel="canonical" href="/rust"></head>
          <body><article><p>Enough words here that extraction still has a main block to pick from for this canonical URL test case.</p></article></body></html>"#;
        let page = extract_page(html, "https://example.com/blog/post");
        assert_eq!(
            page.canonical_url.as_deref(),
            Some("https://example.com/rust")
        );
    }
}
