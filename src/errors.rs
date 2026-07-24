//! Error taxonomy and secret hygiene.
//!
//! The feed URL works as a bearer credential — only scheme + hostname may ever
//! appear in logs, error messages, or tool output.

use std::fmt;

#[derive(Debug)]
pub enum AppError {
    Config(String),
    FeedFetch { masked: String, detail: String },
    FeedTruncated { bytes: usize },
    InvalidRange(String),
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Config(_) => "CONFIG",
            AppError::FeedFetch { .. } => "FEED_FETCH",
            AppError::FeedTruncated { .. } => "FEED_TRUNCATED",
            AppError::InvalidRange(_) => "INVALID_RANGE",
        }
    }

    pub fn message(&self) -> String {
        match self {
            AppError::Config(details) => format!("Invalid configuration — {details}"),
            AppError::FeedFetch { masked, detail } => {
                format!("Failed to fetch the ICS feed from {masked}: {detail}")
            }
            AppError::FeedTruncated { bytes } => format!(
                "ICS feed appears truncated ({bytes} bytes received, does not end with \
                 END:VCALENDAR); refusing to serve partial calendar data"
            ),
            AppError::InvalidRange(message) => message.clone(),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for AppError {}

/// Minimal URL splitter — deliberately not the `url` crate (idna/percent
/// machinery is the single biggest avoidable dependency). Only the shapes the
/// masking/sanitizing paths need; pinned by the ported config.spec cases.
pub(crate) struct UrlParts<'a> {
    pub scheme: String,
    pub username: &'a str,
    pub password: &'a str,
    pub host: String,
    /// Path + query (fragment stripped); empty when the URL has neither.
    pub path_and_query: &'a str,
}

pub(crate) fn parse_url(url: &str) -> Option<UrlParts<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return None;
    }
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let after = &rest[auth_end..];

    // Userinfo splits on the LAST '@' of the authority (RFC 3986).
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (&authority[..i], &authority[i + 1..]),
        None => ("", authority),
    };
    let (username, password) = match userinfo.split_once(':') {
        Some((u, p)) => (u, p),
        None => (userinfo, ""),
    };
    let host = if hostport.starts_with('[') {
        // Bracketed IPv6 literal, keep the brackets like URL.hostname does.
        &hostport[..=hostport.find(']')?]
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return None;
    }
    let path_and_query = after.split('#').next().unwrap_or("");
    Some(UrlParts {
        scheme: scheme.to_ascii_lowercase(),
        username,
        password,
        host: host.to_ascii_lowercase(),
        path_and_query,
    })
}

/// Only scheme + hostname survive; userinfo, port, path, and query are
/// dropped. Unparseable input masks to a fixed placeholder.
pub fn mask_ics_url(url: &str) -> String {
    match parse_url(url) {
        Some(p) => format!("{}://{}/…", p.scheme, p.host),
        None => "<invalid url>".to_string(),
    }
}

/// Scrub any occurrence of the feed URL (or its path/query/credentials) from
/// arbitrary text before it reaches stderr or a tool result. Third-party
/// errors can embed the full URL in messages and cause chains.
pub fn sanitize_text(text: &str, ics_url: &str) -> String {
    let mut out = text.replace(ics_url, &mask_ics_url(ics_url));
    if let Some(p) = parse_url(ics_url) {
        if p.path_and_query.len() > 1 {
            out = out.replace(p.path_and_query, "/…");
        }
        if !p.username.is_empty() {
            out = out.replace(p.username, "…");
        }
        if !p.password.is_empty() {
            out = out.replace(p.password, "…");
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ICS_URL: &str =
        "https://outlook.office365.com/owa/calendar/abc123secret/reachcalendar.ics";

    #[test]
    fn mask_keeps_scheme_and_hostname_only() {
        assert_eq!(mask_ics_url(ICS_URL), "https://outlook.office365.com/…");
    }

    #[test]
    fn mask_drops_userinfo_path_and_query() {
        assert_eq!(
            mask_ics_url("https://user:pass@host.example/secret/path?token=abc"),
            "https://host.example/…"
        );
    }

    #[test]
    fn mask_tolerates_garbage() {
        assert_eq!(mask_ics_url("not a url"), "<invalid url>");
    }

    #[test]
    fn sanitize_replaces_the_full_url() {
        assert_eq!(
            sanitize_text(&format!("fetch failed for {ICS_URL} (500)"), ICS_URL),
            "fetch failed for https://outlook.office365.com/… (500)"
        );
    }

    #[test]
    fn sanitize_replaces_bare_path_and_query_when_host_is_absent() {
        let text = "GET /owa/calendar/abc123secret/reachcalendar.ics returned 404";
        assert_eq!(sanitize_text(text, ICS_URL), "GET /… returned 404");
    }

    #[test]
    fn sanitize_scrubs_credentials() {
        let url = "https://user:hunter2@host.example/cal.ics";
        assert_eq!(
            sanitize_text("auth hunter2 rejected", url),
            "auth … rejected"
        );
    }

    #[test]
    fn error_messages_match_the_ts_formats() {
        assert_eq!(
            AppError::FeedFetch {
                masked: "https://host.example/…".into(),
                detail: "HTTP 404".into(),
            }
            .message(),
            "Failed to fetch the ICS feed from https://host.example/…: HTTP 404"
        );
        assert_eq!(
            AppError::FeedTruncated { bytes: 1234 }.message(),
            "ICS feed appears truncated (1234 bytes received, does not end with END:VCALENDAR); \
             refusing to serve partial calendar data"
        );
        assert_eq!(AppError::Config("X is required".into()).code(), "CONFIG");
    }
}
