//! Feed download with a single-slot TTL cache. Fails hard on errors — an
//! expired snapshot is never served stale (user decision, 2026-07-14).
//!
//! Every error built here uses the masked host only; third-party fetch errors
//! (which can embed the URL) are reduced to an enum before any formatting —
//! `Display` on a ureq error is the secret-leak vector and never runs.
//!
//! Single-flight de-duplication is structural in this port: tool calls are
//! serialized (stdio: one thread; HTTP: a mutex around the server state), so
//! concurrent fetches cannot happen by construction.

use crate::config::Config;
use crate::errors::{AppError, mask_ics_url};

#[derive(Debug)]
pub struct FeedSnapshot {
    /// UTF-8 byte length of `raw`.
    pub bytes: usize,
    /// Whether the payload ends with END:VCALENDAR — a truncated download is
    /// exactly the failure mode this server exists to fix.
    pub ends_valid: bool,
    pub fetched_at_ms: i64,
    pub raw: String,
}

pub struct HttpOk {
    pub status: u16,
    pub body: String,
}

#[derive(Debug)]
pub enum HttpErr {
    Timeout,
    Io(std::io::ErrorKind),
    Other,
}

/// Injectable HTTP GET (tests pass stubs; prod passes [`UreqTransport`]).
pub trait Transport: Send {
    fn get(&self, url: &str) -> Result<HttpOk, HttpErr>;
}

pub trait Clock: Send {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

pub struct FeedClient {
    cfg: Config,
    transport: Box<dyn Transport>,
    clock: Box<dyn Clock>,
    slot: Option<FeedSnapshot>,
}

impl FeedClient {
    pub fn new(
        cfg: Config,
        transport: impl Transport + 'static,
        clock: impl Clock + 'static,
    ) -> Self {
        Self {
            cfg,
            transport: Box::new(transport),
            clock: Box::new(clock),
            slot: None,
        }
    }

    /// Clock used by this client — exposed so feed_info reports a cache age
    /// consistent with the injected test clock.
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Cached snapshot (fetches when missing, expired, or truncated).
    /// `feed_info` uses this directly — it must be able to report a truncated
    /// feed, not fail on it. A truncated snapshot is stored (so it can be
    /// reported) but never counts as fresh — the next call retries
    /// immediately instead of pinning the failure for a whole TTL.
    pub fn get_snapshot(&mut self) -> Result<&FeedSnapshot, AppError> {
        let ttl_ms = self.cfg.cache_ttl_seconds as i64 * 1000;
        let now = self.clock.now_ms();
        let fresh = self
            .slot
            .as_ref()
            .is_some_and(|s| s.ends_valid && now - s.fetched_at_ms < ttl_ms);
        if !fresh {
            // Fail hard: on fetch failure the `?` propagates and the expired
            // slot stays unreachable (it will never satisfy `fresh`).
            self.slot = Some(self.fetch_snapshot()?);
        }
        match &self.slot {
            Some(snapshot) => Ok(snapshot),
            None => unreachable!("slot was just filled"),
        }
    }

    /// Full raw feed, guaranteed complete — event tools must never work with
    /// a truncated payload.
    pub fn get_validated_raw(&mut self) -> Result<&str, AppError> {
        let snapshot = self.get_snapshot()?;
        if !snapshot.ends_valid {
            return Err(AppError::FeedTruncated {
                bytes: snapshot.bytes,
            });
        }
        Ok(&snapshot.raw)
    }

    fn fetch_snapshot(&self) -> Result<FeedSnapshot, AppError> {
        let masked = mask_ics_url(&self.cfg.ics_url);
        let res = self
            .transport
            .get(&self.cfg.ics_url)
            .map_err(|err| AppError::FeedFetch {
                masked: masked.clone(),
                detail: self.failure_detail(&err),
            })?;
        if !(200..300).contains(&res.status) {
            return Err(AppError::FeedFetch {
                masked,
                detail: format!("HTTP {}", res.status),
            });
        }
        let raw = res.body;
        Ok(FeedSnapshot {
            bytes: raw.len(),
            ends_valid: ends_valid(&raw),
            fetched_at_ms: self.clock.now_ms(),
            raw,
        })
    }

    /// Reduce a fetch failure to a URL-free description.
    fn failure_detail(&self, err: &HttpErr) -> String {
        match err {
            HttpErr::Timeout => format!("timeout after {}ms", self.cfg.fetch_timeout_ms),
            HttpErr::Io(kind) => format!("{kind:?}"),
            HttpErr::Other => "network error".to_string(),
        }
    }
}

/// Port of `/END:VCALENDAR\s*$/i` — trailing whitespace/CRLF tolerated.
fn ends_valid(raw: &str) -> bool {
    const SUFFIX: &str = "END:VCALENDAR";
    let trimmed = raw.trim_end();
    trimmed
        .get(trimmed.len().wrapping_sub(SUFFIX.len())..)
        .is_some_and(|end| end.eq_ignore_ascii_case(SUFFIX))
}

/// Production transport — the only place ureq appears. The agent is built
/// once; connection reuse across cache refreshes is a free bonus.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(timeout_ms: u64) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_millis(timeout_ms)))
            .user_agent("calendar-ics-mcp")
            .build()
            .into();
        Self { agent }
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &str) -> Result<HttpOk, HttpErr> {
        let mut res = self
            .agent
            .get(url)
            .header("accept", "text/calendar, */*")
            .call()
            .map_err(map_ureq_err)?;
        let status = res.status().as_u16();
        // No size limit, matching the TS fetch; decode like WHATWG
        // `response.text()` (UTF-8 with replacement characters).
        let bytes = res
            .body_mut()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()
            .map_err(map_ureq_err)?;
        let body = String::from_utf8_lossy(&bytes).into_owned();
        Ok(HttpOk { status, body })
    }
}

fn map_ureq_err(err: ureq::Error) -> HttpErr {
    match err {
        ureq::Error::Timeout(_) => HttpErr::Timeout,
        ureq::Error::Io(io) => HttpErr::Io(io.kind()),
        _ => HttpErr::Other,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use chrono_tz::Tz;

    use super::*;

    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/feed.ics"
    ));
    const FIXTURE_TRUNCATED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/feed-truncated.ics"
    ));
    const SECRET: &str = "SECRETPATH123";

    fn test_config() -> Config {
        Config {
            cache_ttl_seconds: 300,
            fetch_timeout_ms: 15_000,
            ics_url: "https://feedhost.example.com/owa/calendar/SECRETPATH123/reachcalendar.ics"
                .to_string(),
            tz_default: Tz::Europe__Prague,
            http_bind: None,
            http_bearer_token: None,
        }
    }

    struct SeqTransport {
        calls: Arc<AtomicUsize>,
        #[allow(clippy::type_complexity)]
        responder: Box<dyn Fn(usize) -> Result<HttpOk, HttpErr> + Send>,
    }

    impl Transport for SeqTransport {
        fn get(&self, _url: &str) -> Result<HttpOk, HttpErr> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            (self.responder)(n)
        }
    }

    struct TestClock(Arc<AtomicI64>);

    impl Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct Harness {
        client: FeedClient,
        calls: Arc<AtomicUsize>,
        now: Arc<AtomicI64>,
    }

    impl Harness {
        fn advance(&self, ms: i64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
        }
    }

    fn make_client(
        responder: impl Fn(usize) -> Result<HttpOk, HttpErr> + Send + 'static,
    ) -> Harness {
        let calls = Arc::new(AtomicUsize::new(0));
        let now = Arc::new(AtomicI64::new(1_000_000));
        let client = FeedClient::new(
            test_config(),
            SeqTransport {
                calls: Arc::clone(&calls),
                responder: Box::new(responder),
            },
            TestClock(Arc::clone(&now)),
        );
        Harness { client, calls, now }
    }

    fn ok(body: &'static str) -> Result<HttpOk, HttpErr> {
        Ok(HttpOk {
            status: 200,
            body: body.to_string(),
        })
    }

    // The TS suite's "deduplicates concurrent fetches (single-flight)" test
    // has no counterpart here: tool calls are serialized (&mut self on one
    // thread; a mutex in HTTP mode), so concurrent fetches are impossible by
    // construction rather than by promise bookkeeping.

    #[test]
    fn serves_from_cache_within_the_ttl() {
        let mut h = make_client(|_| ok(FIXTURE));
        h.client.get_snapshot().unwrap();
        h.client.get_snapshot().unwrap();
        assert_eq!(h.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn refetches_after_the_ttl_expires() {
        let mut h = make_client(|_| ok(FIXTURE));
        h.client.get_snapshot().unwrap();
        h.advance(299_000);
        h.client.get_snapshot().unwrap();
        assert_eq!(h.calls.load(Ordering::SeqCst), 1);
        h.advance(2_000); // 301 s total — past the 300 s TTL
        h.client.get_snapshot().unwrap();
        assert_eq!(h.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn does_not_serve_an_expired_snapshot_when_the_refetch_fails() {
        let mut h = make_client(|n| {
            if n == 0 {
                ok(FIXTURE)
            } else {
                Ok(HttpOk {
                    status: 404,
                    body: "gone".to_string(),
                })
            }
        });
        h.client.get_snapshot().unwrap();
        h.advance(301_000);
        let err = h.client.get_snapshot().unwrap_err();
        assert_eq!(err.code(), "FEED_FETCH");
    }

    #[test]
    fn maps_http_errors_without_leaking_the_url_path() {
        let mut h = make_client(|_| {
            Ok(HttpOk {
                status: 404,
                body: "nope".to_string(),
            })
        });
        let msg = h.client.get_snapshot().unwrap_err().message();
        assert!(msg.contains("HTTP 404"));
        assert!(msg.contains("https://feedhost.example.com/…"));
        assert!(!msg.contains(SECRET));
    }

    #[test]
    fn maps_timeouts_to_a_readable_message() {
        let mut h = make_client(|_| Err(HttpErr::Timeout));
        let msg = h.client.get_snapshot().unwrap_err().message();
        assert!(msg.contains("timeout after 15000ms"));
    }

    #[test]
    fn maps_network_errors_to_the_io_kind_never_a_raw_message() {
        let mut h = make_client(|_| Err(HttpErr::Io(std::io::ErrorKind::ConnectionRefused)));
        let msg = h.client.get_snapshot().unwrap_err().message();
        assert!(msg.contains("ConnectionRefused"));
        assert!(!msg.contains(SECRET));
    }

    #[test]
    fn ac7_flags_a_truncated_feed_and_refuses_to_serve_it_to_event_tools() {
        let mut h = make_client(|_| ok(FIXTURE_TRUNCATED));
        assert!(!h.client.get_snapshot().unwrap().ends_valid);
        let err = h.client.get_validated_raw().unwrap_err();
        assert_eq!(err.code(), "FEED_TRUNCATED");
        assert!(err.message().contains("truncated"));
    }

    #[test]
    fn ac7_does_not_cache_a_truncated_snapshot_recovery_is_immediate() {
        let mut h = make_client(|n| ok(if n == 0 { FIXTURE_TRUNCATED } else { FIXTURE }));
        assert!(h.client.get_validated_raw().is_err());
        // The feed recovered upstream; the very next call must succeed.
        assert_eq!(h.client.get_validated_raw().unwrap(), FIXTURE);
        assert_eq!(h.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn ac7_accepts_a_complete_feed_with_trailing_crlf() {
        let mut h = make_client(|_| ok(FIXTURE));
        {
            let snapshot = h.client.get_snapshot().unwrap();
            assert!(snapshot.ends_valid);
            assert_eq!(snapshot.bytes, FIXTURE.len());
        }
        assert_eq!(h.client.get_validated_raw().unwrap(), FIXTURE);
    }
}
