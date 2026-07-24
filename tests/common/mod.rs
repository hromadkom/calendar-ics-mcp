//! Port of tests/helpers.ts — shared fixtures and expansion helpers.

#![allow(dead_code, clippy::unwrap_used)]

use std::sync::LazyLock;

use chrono_tz::Tz;

use calendar_ics_mcp::config::Config;
use calendar_ics_mcp::ics::expand::{ExpandedInstance, day_window, expand_events};
use calendar_ics_mcp::ics::model::ParsedCalendar;
use calendar_ics_mcp::ics::parse::parse_calendar;
use calendar_ics_mcp::ics::tzids::{normalize_tzids, unfold_ics};

pub const TZ: Tz = Tz::Europe__Prague;

pub const FIXTURE: &str = include_str!("../fixtures/feed.ics");
pub const FIXTURE_TRUNCATED: &str = include_str!("../fixtures/feed-truncated.ics");

/// The path segment doubles as the "secret" that must never leak into output.
pub const TEST_ICS_URL: &str =
    "https://feedhost.example.com/owa/calendar/SECRETPATH123/reachcalendar.ics";

pub fn test_config() -> Config {
    Config {
        cache_ttl_seconds: 300,
        fetch_timeout_ms: 15_000,
        ics_url: TEST_ICS_URL.to_string(),
        tz_default: TZ,
        http_bind: None,
        http_bearer_token: None,
    }
}

pub static PARSED: LazyLock<ParsedCalendar> = LazyLock::new(|| {
    let normalized = normalize_tzids(&unfold_ics(FIXTURE), TZ);
    parse_calendar(&normalized.ics, TZ).unwrap()
});

pub fn instances_on(date: &str) -> Vec<ExpandedInstance<'static>> {
    let win = day_window(Some(date), TZ, 0).unwrap();
    expand_events(&PARSED, &win).unwrap()
}

pub fn uids_on(date: &str) -> Vec<&'static str> {
    instances_on(date)
        .into_iter()
        .map(|inst| inst.event.uid.as_str())
        .collect()
}

pub fn instance_of(date: &str, uid: &str) -> Option<ExpandedInstance<'static>> {
    instances_on(date).into_iter().find(|i| i.event.uid == uid)
}

pub fn utc_ms(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .unwrap()
        .timestamp_millis()
}

/// Start instant of a timed instance (panics on all-day — tests only).
pub fn start_ms(inst: &ExpandedInstance<'_>) -> i64 {
    match inst.time {
        calendar_ics_mcp::ics::expand::InstanceTime::Timed { start_ms, .. } => start_ms,
        calendar_ics_mcp::ics::expand::InstanceTime::AllDay { .. } => {
            panic!("expected a timed instance")
        }
    }
}
