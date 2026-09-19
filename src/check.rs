//! Deciding whether one URL is actually broken.
//!
//! The distinction that matters: a 403 from a bot-protected host is not a
//! broken link, and reporting it as one is how these tools lose trust. Anything
//! we cannot verify is reported as unverified, separately from anything we know
//! is dead.

use std::error::Error as _; // for reqwest::Error::source() in describe()
use std::net::{IpAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use url::Url;

use crate::extract;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 2xx - the page is there.
    Ok,
    /// 404 / 410 - certainly dead.
    Dead,
    /// 5xx - the server is failing, may be temporary but the visitor still sees an error.
    ServerError,
    /// 401 / 403 / 429 / 999 - we were refused, a human with a browser may be fine.
    Refused,
    /// Timed out, DNS failure, TLS failure - could not be checked.
    Unreachable,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Dead => "dead",
            Verdict::ServerError => "server error",
            Verdict::Refused => "refused (bot protection?)",
            Verdict::Unreachable => "unreachable",
        }
    }

    /// Only these two are worth waking someone up for.
    pub fn is_problem(self) -> bool {
        matches!(self, Verdict::Dead | Verdict::ServerError)
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub url: Url,
    pub status: Option<u16>,
    pub verdict: Verdict,
    pub final_url: Option<Url>,
    pub detail: String,
    pub millis: u128,
}

impl Outcome {
    /// A redirect that lands somewhere else entirely is worth knowing about
    /// even when it technically works.
    pub fn redirected_offsite(&self) -> bool {
        match (&self.final_url, self.verdict) {
            (Some(final_url), Verdict::Ok) => !crate::extract::same_site(&self.url, final_url),
            _ => false,
        }
    }
}

fn classify(status: StatusCode) -> Verdict {
    match status.as_u16() {
        200..=299 => Verdict::Ok,
        404 | 410 => Verdict::Dead,
        401 | 403 | 429 | 999 => Verdict::Refused,
        500..=599 => Verdict::ServerError,
        // 3xx here means the redirect limit was hit; everything else unusual
        // is reported as unverified rather than guessed at.
        _ => Verdict::Unreachable,
    }
}

/// True for `https://host/` and `https://host` - nothing but the domain.
fn is_site_root(url: &Url) -> bool {
    matches!(url.path(), "" | "/") && url.query().is_none()
}


fn describe(error: &reqwest::Error) -> (Verdict, String) {
    if error.is_timeout() {
        (Verdict::Unreachable, "timed out".into())
    } else if error.is_connect() {
        (Verdict::Unreachable, "connection failed (DNS or TLS)".into())
    } else if error.is_redirect() {
        // Covers both "too many redirects" and our own refusal (via
        // Policy::custom in client(), below) to follow a redirect into an
        // internal address - reqwest tags both as a redirect error, so
        // surface the real reason instead of a one-size-fits-all guess.
        (Verdict::Unreachable, error.source().map(|source| source.to_string()).unwrap_or_else(|| error.to_string()))
    } else {
        (Verdict::Unreachable, error.to_string())
    }
}

/// Refuse to send a request toward an address a crawled page has no business
/// sending us to. Resolution happens here, right before connecting, because
/// that is the only place a hostname's *current* address is actually known -
/// checking the URL text alone would miss both a literal internal IP and DNS
/// rebinding (a name that resolves differently by the time we dial it).
pub fn host_guard(url: &Url, allow_internal: bool) -> Result<(), String> {
    if allow_internal {
        return Ok(());
    }
    let Some(host) = url.host_str() else {
        return Err("URL has no host".to_string());
    };
    // 80 only matters as a placeholder for resolution below; the port never
    // affects which addresses a hostname resolves to.
    let port = url.port_or_known_default().unwrap_or(80);

    // A literal IP needs no DNS lookup - and to_socket_addrs() on one can
    // still fail in odd environments, so it is checked directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if extract::is_blocked_address(ip) {
            Err(format!("{host} is an internal address - refusing to fetch it (use --allow-internal if this is intentional)"))
        } else {
            Ok(())
        };
    }

    // A hostname is resolved for real, because that is the address we are
    // actually about to connect to, not a guess based on its spelling. Every
    // resolved address is checked, not just the first, so a name that
    // answers with a mix of public and internal addresses cannot sneak the
    // internal one through.
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("could not resolve {host}: {error}"))?;
    for addr in addrs {
        if extract::is_blocked_address(addr.ip()) {
            return Err(format!(
                "{host} resolves to the internal address {} - refusing to fetch it (use --allow-internal if this is intentional)",
                addr.ip()
            ));
        }
    }
    Ok(())
}

pub fn client(timeout: u64, user_agent: &str, allow_internal: bool) -> reqwest::Result<Client> {
    Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(timeout))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            // Mirrors Policy::limited(5), which this replaces - only the
            // destination check below is new.
            if attempt.previous().len() > 5 {
                return attempt.error("too many redirects");
            }
            match host_guard(attempt.url(), allow_internal) {
                Ok(()) => attempt.follow(),
                Err(reason) => attempt.error(reason),
            }
        }))
        .build()
}

/// Cap on a response body, applied to the *decoded* (post-gzip) bytes as
/// they stream in. Content-Length is attacker-controlled and, with gzip
/// transport compression, bears no relation to the decompressed size we are
/// about to hold in memory - trusting it is how a hostile server turns a few
/// compressed kilobytes into a multi-gigabyte allocation. 32 MB is far more
/// than any real page needs, and small enough that a decompression bomb
/// cannot turn one crawled page into an out-of-memory crash.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Read a response body up to `cap` bytes, decoded as UTF-8 (lossily - a
/// body that is not valid text still has to fail closed here, not panic).
/// Reads incrementally and checks the running total after each chunk, so a
/// hostile body is caught as it grows rather than after it has already been
/// buffered in full.
pub async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<String, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > cap {
            return Err(format!(
                "response body is over {} MB after decompression - refusing to read further (looks like a decompression bomb)",
                cap / 1_048_576
            ));
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// HEAD first (cheap), GET as a fallback.
///
/// Plenty of servers answer HEAD with 405 or 403 while serving GET perfectly -
/// treating that as broken would fill the report with noise.
pub async fn check(client: &Client, url: Url, allow_internal: bool) -> Outcome {
    let started = Instant::now();

    if let Err(reason) = host_guard(&url, allow_internal) {
        return Outcome {
            status: None,
            verdict: Verdict::Unreachable,
            final_url: None,
            detail: reason,
            url,
            millis: started.elapsed().as_millis(),
        };
    }

    let mut response = client.head(url.clone()).send().await;
    let head_status = response.as_ref().ok().map(|r| r.status().as_u16());
    let needs_get = match &response {
        Ok(r) => matches!(r.status().as_u16(), 400..=499),
        Err(_) => true,
    };
    if needs_get {
        response = client.get(url.clone()).send().await;
    }

    match response {
        Ok(r) => {
            let status = r.status();
            let final_url = r.url().clone();
            let mut verdict = classify(status);
            let mut detail = status.canonical_reason().unwrap_or("").to_string();

            // Bot protection that lies. crates.io, for one, answers HEAD with
            // 403 and GET with 404 while being perfectly alive in a browser.
            // A refusal followed by a 404 is a filter, not a dead page - and
            // calling a live page dead is the one mistake that makes a report
            // worthless.
            if verdict == Verdict::Dead && matches!(head_status, Some(401 | 403 | 429 | 999)) {
                verdict = Verdict::Refused;
                detail = format!("HEAD {} then GET 404 - looks like bot protection", head_status.unwrap());
            }

            // A 404 on the root of a domain is not a deleted page. Either the
            // whole site is gone (and then the connection fails, not the
            // status) or a filter is answering us. crates.io does exactly this:
            // 404 with an empty body to anything that is not a browser.
            if verdict == Verdict::Dead && is_site_root(&url) {
                verdict = Verdict::Refused;
                detail = "404 on the site root - a live domain does not do that to a browser".into();
            }

            Outcome {
                status: Some(status.as_u16()),
                verdict,
                detail,
                final_url: Some(final_url),
                url,
                millis: started.elapsed().as_millis(),
            }
        }
        Err(error) => {
            let (verdict, detail) = describe(&error);
            Outcome {
                url,
                status: None,
                verdict,
                final_url: None,
                detail,
                millis: started.elapsed().as_millis(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16) -> StatusCode {
        StatusCode::from_u16(code).unwrap()
    }

    #[test]
    fn success_codes_are_ok() {
        for code in [200, 201, 204, 226] {
            assert_eq!(classify(status(code)), Verdict::Ok, "{code}");
        }
    }

    #[test]
    fn only_404_and_410_count_as_dead() {
        assert_eq!(classify(status(404)), Verdict::Dead);
        assert_eq!(classify(status(410)), Verdict::Dead);
        assert_ne!(classify(status(403)), Verdict::Dead);
    }

    #[test]
    fn bot_protection_is_refused_not_dead() {
        for code in [401, 403, 429, 999] {
            assert_eq!(classify(status(code)), Verdict::Refused, "{code}");
        }
    }

    #[test]
    fn server_errors_are_their_own_category() {
        for code in [500, 502, 503, 504] {
            assert_eq!(classify(status(code)), Verdict::ServerError, "{code}");
        }
    }

    #[test]
    fn a_site_root_is_recognised() {
        assert!(is_site_root(&Url::parse("https://crates.io/").unwrap()));
        assert!(is_site_root(&Url::parse("https://crates.io").unwrap()));
        assert!(!is_site_root(&Url::parse("https://crates.io/crates/serde").unwrap()));
        assert!(!is_site_root(&Url::parse("https://crates.io/?q=x").unwrap()));
    }

    #[test]
    fn only_dead_and_server_errors_are_reported_as_problems() {
        assert!(Verdict::Dead.is_problem());
        assert!(Verdict::ServerError.is_problem());
        assert!(!Verdict::Refused.is_problem());
        assert!(!Verdict::Unreachable.is_problem());
        assert!(!Verdict::Ok.is_problem());
    }

    #[test]
    fn a_working_link_that_leaves_the_site_is_flagged() {
        let outcome = Outcome {
            url: Url::parse("https://example.com/partner").unwrap(),
            status: Some(200),
            verdict: Verdict::Ok,
            final_url: Some(Url::parse("https://someone-else.com/landing").unwrap()),
            detail: String::new(),
            millis: 10,
        };
        assert!(outcome.redirected_offsite());
    }

    #[test]
    fn an_internal_redirect_is_not_flagged() {
        let outcome = Outcome {
            url: Url::parse("https://example.com/old").unwrap(),
            status: Some(200),
            verdict: Verdict::Ok,
            final_url: Some(Url::parse("https://www.example.com/new").unwrap()),
            detail: String::new(),
            millis: 10,
        };
        assert!(!outcome.redirected_offsite());
    }

    #[test]
    fn host_guard_refuses_a_loopback_target() {
        let url = Url::parse("http://127.0.0.1/admin").unwrap();
        let result = host_guard(&url, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_lowercase().contains("internal"));
    }

    #[test]
    fn host_guard_allows_a_loopback_target_when_allow_internal_is_set() {
        let url = Url::parse("http://127.0.0.1/admin").unwrap();
        assert!(host_guard(&url, true).is_ok());
    }

    #[test]
    fn host_guard_allows_an_ordinary_public_ip_literal() {
        let url = Url::parse("http://93.184.216.34/").unwrap();
        assert!(host_guard(&url, false).is_ok());
    }

    /// Bare-bones HTTP/1.1 server bound to loopback: accepts connections and
    /// sends back exactly the raw bytes it is given (status line, headers,
    /// blank line, body - the caller builds all of it), recording whether it
    /// was ever hit. Enough to exercise real reqwest behaviour (redirects,
    /// gzip) without a mock-server dependency.
    struct Loopback {
        addr: std::net::SocketAddr,
        hit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Loopback {
        fn start(response: Vec<u8>) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free loopback port");
            let addr = listener.local_addr().expect("listener has a local address");
            let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let hit_thread = hit.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    hit_thread.store(true, std::sync::atomic::Ordering::SeqCst);
                    let mut buffer = [0u8; 512];
                    let _ = stream.read(&mut buffer);
                    let _ = stream.write_all(&response);
                }
            });
            Loopback { addr, hit }
        }

        fn was_hit(&self) -> bool {
            self.hit.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn ok_response() -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".to_vec()
    }

    #[tokio::test]
    async fn checking_a_loopback_target_never_opens_a_connection() {
        let server = Loopback::start(ok_response());
        let url = Url::parse(&format!("http://{}/", server.addr)).unwrap();

        let http_client = client(5, "linkscan-test", false).unwrap();
        let outcome = check(&http_client, url, false).await;

        assert_ne!(outcome.verdict, Verdict::Ok, "a loopback target must be refused, not fetched");
        assert!(!server.was_hit(), "the guard must refuse before ever opening a connection");
    }

    #[tokio::test]
    async fn a_gzip_body_that_inflates_past_the_cap_is_rejected_not_fully_read() {
        use std::io::Write;

        // 40 MB of zeros compresses to almost nothing under gzip, so this is
        // a realistic decompression bomb: tiny over the wire, huge once
        // reqwest's transparent gzip decoding inflates it back out.
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&vec![0u8; 40 * 1024 * 1024]).expect("compress payload");
        let compressed = encoder.finish().expect("finish gzip stream");

        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            compressed.len()
        )
        .into_bytes();
        response.extend_from_slice(&compressed);

        let server = Loopback::start(response);
        let url = Url::parse(&format!("http://{}/", server.addr)).unwrap();
        let http_client = client(5, "linkscan-test", false).unwrap();
        let raw_response = http_client.get(url).send().await.expect("request should succeed at the HTTP level");

        let result = read_capped(raw_response, 32 * 1024 * 1024).await;
        assert!(result.is_err(), "a body that inflates past the cap must be rejected, not read in full");
    }
}
