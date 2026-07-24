//! Configuration, parsed and validated from the environment once at startup.
//! Nothing else in the codebase reads the environment.
//!
//! Error messages must never echo values back — ICS_URL is a secret.

use chrono_tz::Tz;

use crate::errors::{AppError, parse_url};
use crate::ics::tzids::is_valid_iana;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub cache_ttl_seconds: u64,
    pub fetch_timeout_ms: u64,
    pub ics_url: String,
    pub tz_default: Tz,
    /// Set → serve MCP Streamable HTTP on this address; unset → stdio.
    pub http_bind: Option<std::net::SocketAddr>,
    /// Optional bearer token required on `POST /mcp` (never logged/echoed).
    pub http_bearer_token: Option<String>,
}

/// Parse configuration through an injected getter (prod:
/// `|k| std::env::var(k).ok()`). A value that trims to empty counts as unset,
/// matching the TS `emptyToUndefined` preprocessor. All issues are collected
/// and reported together, in the TS zod schema's key order.
pub fn load_config(get: impl Fn(&str) -> Option<String>) -> Result<Config, AppError> {
    let var = |key: &str| get(key).filter(|v| !v.trim().is_empty());
    let mut issues: Vec<&'static str> = Vec::new();

    let cache_ttl_seconds = match var("CACHE_TTL_SECONDS") {
        None => 300,
        Some(v) => v.trim().parse::<u64>().unwrap_or_else(|_| {
            issues.push("CACHE_TTL_SECONDS must be a non-negative integer");
            0
        }),
    };

    let fetch_timeout_ms = match var("FETCH_TIMEOUT_MS") {
        None => 15_000,
        Some(v) => match v.trim().parse::<u64>() {
            Ok(n) if n >= 1 => n,
            _ => {
                issues.push("FETCH_TIMEOUT_MS must be a positive integer");
                0
            }
        },
    };

    let ics_url = match var("ICS_URL") {
        None => {
            issues.push("ICS_URL is required");
            String::new()
        }
        Some(v) => {
            let scheme_ok =
                parse_url(&v).is_some_and(|p| p.scheme == "http" || p.scheme == "https");
            if scheme_ok {
                v
            } else {
                issues.push("ICS_URL must be an http(s) URL");
                String::new()
            }
        }
    };

    let tz_default = match var("TZ_DEFAULT") {
        None => Tz::Europe__Prague,
        Some(v) => is_valid_iana(&v).unwrap_or_else(|| {
            issues.push("TZ_DEFAULT must be a valid IANA timezone");
            Tz::UTC
        }),
    };

    let http_bind = match var("HTTP_BIND") {
        None => None,
        Some(v) => match v.trim().parse::<std::net::SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(_) => {
                issues.push("HTTP_BIND must be an <ip>:<port> socket address");
                None
            }
        },
    };

    let http_bearer_token = var("HTTP_BEARER_TOKEN");

    if !issues.is_empty() {
        return Err(AppError::Config(issues.join("; ")));
    }
    Ok(Config {
        cache_ttl_seconds,
        fetch_timeout_ms,
        ics_url,
        tz_default,
        http_bind,
        http_bearer_token,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ICS_URL: &str =
        "https://outlook.office365.com/owa/calendar/abc123secret/reachcalendar.ics";

    fn load(vars: &[(&str, &str)]) -> Result<Config, AppError> {
        load_config(|k| {
            vars.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_string())
        })
    }

    #[test]
    fn applies_defaults_when_only_ics_url_is_set() {
        let cfg = load(&[("ICS_URL", ICS_URL)]).unwrap();
        assert_eq!(
            cfg,
            Config {
                cache_ttl_seconds: 300,
                fetch_timeout_ms: 15_000,
                ics_url: ICS_URL.to_string(),
                tz_default: Tz::Europe__Prague,
                http_bind: None,
                http_bearer_token: None,
            }
        );
    }

    #[test]
    fn parses_http_bind_and_rejects_garbage_without_echoing() {
        let cfg = load(&[("HTTP_BIND", "0.0.0.0:8590"), ("ICS_URL", ICS_URL)]).unwrap();
        assert_eq!(
            cfg.http_bind,
            Some("0.0.0.0:8590".parse::<std::net::SocketAddr>().unwrap())
        );
        let err =
            load(&[("HTTP_BIND", "secret-host.example:x"), ("ICS_URL", ICS_URL)]).unwrap_err();
        assert!(err.message().contains("HTTP_BIND"));
        assert!(!err.message().contains("secret-host"));
    }

    #[test]
    fn accepts_an_optional_bearer_token() {
        let cfg = load(&[("HTTP_BEARER_TOKEN", "sekret"), ("ICS_URL", ICS_URL)]).unwrap();
        assert_eq!(cfg.http_bearer_token.as_deref(), Some("sekret"));
    }

    #[test]
    fn accepts_explicit_overrides() {
        let cfg = load(&[
            ("CACHE_TTL_SECONDS", "60"),
            ("FETCH_TIMEOUT_MS", "5000"),
            ("ICS_URL", ICS_URL),
            ("TZ_DEFAULT", "Europe/Berlin"),
        ])
        .unwrap();
        assert_eq!(cfg.cache_ttl_seconds, 60);
        assert_eq!(cfg.fetch_timeout_ms, 5000);
        assert_eq!(cfg.tz_default, Tz::Europe__Berlin);
    }

    #[test]
    fn fails_without_ics_url() {
        let err = load(&[]).unwrap_err();
        assert_eq!(err.code(), "CONFIG");
        assert!(err.message().contains("ICS_URL is required"));
        let err = load(&[("ICS_URL", "")]).unwrap_err();
        assert!(err.message().contains("ICS_URL"));
    }

    #[test]
    fn rejects_non_http_ics_url_without_echoing_its_value() {
        let err = load(&[("ICS_URL", "ftp://secret-host.example/private-path")]).unwrap_err();
        assert_eq!(err.code(), "CONFIG");
        let msg = err.message();
        assert!(msg.contains("ICS_URL must be an http(s) URL"));
        assert!(!msg.contains("secret-host"));
        assert!(!msg.contains("private-path"));
    }

    #[test]
    fn rejects_an_invalid_tz_default() {
        let err = load(&[("ICS_URL", ICS_URL), ("TZ_DEFAULT", "Mars/Olympus_Mons")]).unwrap_err();
        assert!(err.message().contains("TZ_DEFAULT"));
    }

    #[test]
    fn rejects_a_non_numeric_cache_ttl() {
        let err = load(&[("CACHE_TTL_SECONDS", "soon"), ("ICS_URL", ICS_URL)]).unwrap_err();
        assert!(err.message().contains("CACHE_TTL_SECONDS"));
    }

    #[test]
    fn rejects_a_negative_fetch_timeout() {
        let err = load(&[("FETCH_TIMEOUT_MS", "-1"), ("ICS_URL", ICS_URL)]).unwrap_err();
        assert!(err.message().contains("FETCH_TIMEOUT_MS"));
    }

    #[test]
    fn blank_values_fall_back_to_defaults() {
        let cfg = load(&[
            ("CACHE_TTL_SECONDS", "  "),
            ("ICS_URL", ICS_URL),
            ("TZ_DEFAULT", ""),
        ])
        .unwrap();
        assert_eq!(cfg.cache_ttl_seconds, 300);
        assert_eq!(cfg.tz_default, Tz::Europe__Prague);
    }
}
