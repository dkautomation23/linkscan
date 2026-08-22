//! Pulling links out of one HTML page.
//!
//! Kept separate from the async crawler for a practical reason: `scraper`'s
//! `Html` is not `Send`, so it must never be alive across an `.await`. Doing
//! the parsing in a plain function returning owned data makes that impossible
//! to get wrong.

use std::collections::HashSet;
use url::Url;

/// Where a link was found and what kind of thing it points at.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Found {
    pub url: Url,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Anchor,
    Image,
    Script,
    Stylesheet,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Anchor => "link",
            Kind::Image => "image",
            Kind::Script => "script",
            Kind::Stylesheet => "stylesheet",
        }
    }
}

/// Schemes we have no business fetching.
fn is_fetchable(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// Resolve one href against the page it appeared on, dropping fragments.
///
/// `#pricing` and `/pricing#top` point at the same document, so keeping the
/// fragment would mean checking the same URL several times.
pub fn resolve(base: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let mut url = base.join(href).ok()?;
    if !is_fetchable(&url) {
        return None;
    }
    url.set_fragment(None);
    Some(url)
}

/// Every unique link on the page, in document order.
pub fn links(html: &str, base: &Url) -> Vec<Found> {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let mut seen: HashSet<Found> = HashSet::new();
    let mut out = Vec::new();

    let sources: [(&str, &str, Kind); 4] = [
        ("a[href]", "href", Kind::Anchor),
        ("img[src]", "src", Kind::Image),
        ("script[src]", "src", Kind::Script),
        ("link[rel='stylesheet'][href]", "href", Kind::Stylesheet),
    ];

    for (selector, attribute, kind) in sources {
        let Ok(parsed) = Selector::parse(selector) else {
            continue;
        };
        for element in document.select(&parsed) {
            let Some(value) = element.value().attr(attribute) else {
                continue;
            };
            let Some(url) = resolve(base, value) else {
                continue;
            };
            let found = Found { url, kind };
            if seen.insert(found.clone()) {
                out.push(found);
            }
        }
    }
    out
}

/// Same registrable host, ignoring a leading `www.`.
pub fn same_site(a: &Url, b: &Url) -> bool {
    fn host(url: &Url) -> String {
        url.host_str()
            .unwrap_or_default()
            .trim_start_matches("www.")
            .to_ascii_lowercase()
    }
    host(a) == host(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/docs/guide").unwrap()
    }

    #[test]
    fn relative_links_resolve_against_the_page() {
        let url = resolve(&base(), "../pricing").unwrap();
        assert_eq!(url.as_str(), "https://example.com/pricing");
    }

    #[test]
    fn fragments_are_stripped_so_a_page_is_checked_once() {
        let a = resolve(&base(), "/pricing#plans").unwrap();
        let b = resolve(&base(), "/pricing").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn in_page_anchors_and_empty_hrefs_are_skipped() {
        assert!(resolve(&base(), "#top").is_none());
        assert!(resolve(&base(), "   ").is_none());
    }

    #[test]
    fn non_http_schemes_are_skipped() {
        for href in ["mailto:hi@example.com", "tel:+123", "javascript:void(0)", "data:text/plain,x"] {
            assert!(resolve(&base(), href).is_none(), "{href} should be ignored");
        }
    }

    #[test]
    fn all_four_link_kinds_are_collected() {
        let html = r#"
            <a href="/a">a</a>
            <img src="/logo.png">
            <script src="/app.js"></script>
            <link rel="stylesheet" href="/style.css">
        "#;
        let found = links(html, &base());
        let kinds: Vec<Kind> = found.iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&Kind::Anchor));
        assert!(kinds.contains(&Kind::Image));
        assert!(kinds.contains(&Kind::Script));
        assert!(kinds.contains(&Kind::Stylesheet));
        assert_eq!(found.len(), 4);
    }

    #[test]
    fn the_same_target_is_returned_once() {
        let html = r#"<a href="/pricing">a</a><a href="/pricing#top">b</a><a href="/pricing">c</a>"#;
        assert_eq!(links(html, &base()).len(), 1);
    }

    #[test]
    fn www_and_bare_host_count_as_one_site() {
        let a = Url::parse("https://example.com/x").unwrap();
        let b = Url::parse("https://www.example.com/y").unwrap();
        let c = Url::parse("https://cdn.other.com/z").unwrap();
        assert!(same_site(&a, &b));
        assert!(!same_site(&a, &c));
    }
}
