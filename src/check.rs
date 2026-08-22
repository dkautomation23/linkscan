//! Deciding whether one URL is actually broken.
//!
//! The distinction that matters: a 403 from a bot-protected host is not a
//! broken link, and reporting it as one is how these tools lose trust. Anything
//! we cannot verify is reported as unverified, separately from anything we know
//! is dead.

use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use url::Url;

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
        (Verdict::Unreachable, "too many redirects".into())
    } else {
        (Verdict::Unreachable, error.to_string())
    }
}

pub fn client(timeout: u64, user_agent: &str) -> reqwest::Result<Client> {
    Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(timeout))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
}

/// HEAD first (cheap), GET as a fallback.
///
/// Plenty of servers answer HEAD with 405 or 403 while serving GET perfectly -
/// treating that as broken would fill the report with noise.
pub async fn check(client: &Client, url: Url) -> Outcome {
    let started = Instant::now();

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
}
