//! Pulling links out of one HTML page.
//!
//! Kept separate from the async crawler for a practical reason: `scraper`'s
//! `Html` is not `Send`, so it must never be alive across an `.await`. Doing
//! the parsing in a plain function returning owned data makes that impossible
//! to get wrong.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
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

/// True for an address a crawled page has no business sending this tool
/// toward: loopback, the RFC 1918 private ranges, link-local (this is also
/// where cloud-provider instance metadata lives, e.g. `169.254.169.254`),
/// and IPv6 unique-local. The page being crawled is untrusted input - if it
/// can steer us into any of these, it can turn a link checker into a probe
/// against whatever the process running it can reach.
pub fn is_blocked_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()        // 127.0.0.0/8
        || ip.is_private()  // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local() // 169.254.0.0/16
        || ip.is_unspecified() // 0.0.0.0 - some stacks treat it as localhost
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    // An IPv4-mapped address (::ffff:10.0.0.1) must be judged by the v4
    // rules it carries, not waved through as "not technically private v6".
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_v4(v4);
    }
    let first = ip.segments()[0];
    let link_local = first & 0xffc0 == 0xfe80; // fe80::/10
    let unique_local = first & 0xfe00 == 0xfc00; // fc00::/7
    link_local || unique_local
}

/// Schemes we have no business fetching, plus - when the host is already a
/// literal IP address - addresses we should not even queue. A hostname
/// cannot be judged here without a DNS lookup, and this function stays
/// synchronous on purpose (see the module doc comment); the real, DNS-backed
/// check runs again right before every request in check.rs. This is just the
/// free half of it, so an internal IP spelled out directly in an `href`
/// never even makes it into the crawl queue.
fn is_fetchable(url: &Url, allow_internal: bool) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    if allow_internal {
        return true;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !is_blocked_address(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !is_blocked_address(IpAddr::V6(ip)),
        _ => true,
    }
}

/// Resolve one href against the page it appeared on, dropping fragments.
///
/// `#pricing` and `/pricing#top` point at the same document, so keeping the
/// fragment would mean checking the same URL several times.
pub fn resolve(base: &Url, href: &str, allow_internal: bool) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let mut url = base.join(href).ok()?;
    if !is_fetchable(&url, allow_internal) {
        return None;
    }
    url.set_fragment(None);
    Some(url)
}

/// Every unique link on the page, in document order.
pub fn links(html: &str, base: &Url, allow_internal: bool) -> Vec<Found> {
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
            let Some(url) = resolve(base, value, allow_internal) else {
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

/// Same registrable host AND port, ignoring a leading `www.`. A different
/// port on the same host is treated as a different site on purpose: it is
/// often a different service entirely (an admin panel, a database's HTTP
/// interface) that just happens to share a hostname with the public one.
pub fn same_site(a: &Url, b: &Url) -> bool {
    fn host(url: &Url) -> String {
        url.host_str()
            .unwrap_or_default()
            .trim_start_matches("www.")
            .to_ascii_lowercase()
    }
    host(a) == host(b) && a.port_or_known_default() == b.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/docs/guide").unwrap()
    }

    #[test]
    fn relative_links_resolve_against_the_page() {
        let url = resolve(&base(), "../pricing", false).unwrap();
        assert_eq!(url.as_str(), "https://example.com/pricing");
    }

    #[test]
    fn fragments_are_stripped_so_a_page_is_checked_once() {
        let a = resolve(&base(), "/pricing#plans", false).unwrap();
        let b = resolve(&base(), "/pricing", false).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn in_page_anchors_and_empty_hrefs_are_skipped() {
        assert!(resolve(&base(), "#top", false).is_none());
        assert!(resolve(&base(), "   ", false).is_none());
    }

    #[test]
    fn non_http_schemes_are_skipped() {
        for href in ["mailto:hi@example.com", "tel:+123", "javascript:void(0)", "data:text/plain,x"] {
            assert!(resolve(&base(), href, false).is_none(), "{href} should be ignored");
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
        let found = links(html, &base(), false);
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
        assert_eq!(links(html, &base(), false).len(), 1);
    }

    #[test]
    fn www_and_bare_host_count_as_one_site() {
        let a = Url::parse("https://example.com/x").unwrap();
        let b = Url::parse("https://www.example.com/y").unwrap();
        let c = Url::parse("https://cdn.other.com/z").unwrap();
        assert!(same_site(&a, &b));
        assert!(!same_site(&a, &c));
    }

    #[test]
    fn a_different_port_on_the_same_host_is_a_different_site() {
        // A different port is often a different service entirely (an admin
        // panel, a database's HTTP interface) that just happens to share a
        // hostname with the public site.
        let a = Url::parse("https://example.com/x").unwrap();
        let b = Url::parse("https://example.com:8443/y").unwrap();
        assert!(!same_site(&a, &b));
    }

    #[test]
    fn the_default_port_for_the_scheme_still_counts_as_the_same_site() {
        let a = Url::parse("https://example.com/x").unwrap();
        let b = Url::parse("https://example.com:443/y").unwrap();
        assert!(same_site(&a, &b));
    }

    #[test]
    fn loopback_private_link_local_and_unique_local_addresses_are_blocked() {
        let blocked = [
            "127.0.0.1", "127.53.0.1", "10.1.2.3", "172.16.0.5", "172.31.255.255",
            "192.168.1.1", "169.254.169.254", "0.0.0.0",
            "::1", "::", "fe80::1", "fc00::1", "fd12:3456::1", "::ffff:10.1.2.3",
        ];
        for text in blocked {
            let ip: IpAddr = text.parse().unwrap();
            assert!(is_blocked_address(ip), "{text} should be blocked");
        }
    }

    #[test]
    fn ordinary_public_addresses_are_not_blocked() {
        for text in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            let ip: IpAddr = text.parse().unwrap();
            assert!(!is_blocked_address(ip), "{text} should not be blocked");
        }
    }

    #[test]
    fn a_literal_internal_ip_in_an_href_does_not_resolve() {
        // A crawled page is untrusted input; a hostile page can spell out an
        // internal address directly, without needing a redirect.
        assert!(resolve(&base(), "http://127.0.0.1/admin", false).is_none());
        assert!(resolve(&base(), "http://169.254.169.254/latest/meta-data/", false).is_none());
    }

    #[test]
    fn allow_internal_permits_a_literal_internal_ip_in_an_href() {
        assert!(resolve(&base(), "http://127.0.0.1/admin", true).is_some());
    }
}
