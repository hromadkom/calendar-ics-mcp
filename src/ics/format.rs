//! Shape expanded instances into the tool output format. Timed instants are
//! rendered with the offset of TZ_DEFAULT; all-day events stay date-only.

use std::sync::LazyLock;

use chrono::Utc;
use chrono_tz::Tz;
use regex::Regex;
use serde::Serialize;

use crate::ics::expand::{ExpandedInstance, InstanceTime};
use crate::ics::model::ParsedEvent;

pub const BUSY_STATUSES: [&str; 4] = ["BUSY", "TENTATIVE", "FREE", "OOF"];

const DESCRIPTION_MAX_UTF16_UNITS: usize = 2000;

/// Field order matches the TS object literal — the serialized JSON (and the
/// mirrored `content[0].text`) is byte-identical to the TS output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApiEvent {
    pub all_day: bool,
    pub busy_status: &'static str,
    pub description: Option<String>,
    pub end: String,
    pub is_recurring: bool,
    pub location: Option<String>,
    pub meeting_url: Option<String>,
    pub start: String,
    pub summary: String,
}

/// Everything from the first Teams separator line (a run of underscores) to
/// the end is dial-in/join boilerplate.
static BOILERPLATE_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_{8,}").unwrap_or_else(|_| unreachable!()));

static BLANK_RUNS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").unwrap_or_else(|_| unreachable!()));

static MEETING_URL_PATTERNS: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        r#"(?i)https://teams\.microsoft\.com/l/meetup-join/[^\s<>"']+"#,
        r#"(?i)https://teams\.live\.com/meet/[^\s<>"']+"#,
        r#"(?i)https://[\w.-]*zoom\.us/(?:j|my|s|w)/[^\s<>"']+"#,
        r#"(?i)https://meet\.google\.com/[a-z]{3}-[a-z]{4}-[a-z]{3}[^\s<>"']*"#,
    ]
    .map(|p| Regex::new(p).unwrap_or_else(|_| unreachable!()))
});

/// `2026-03-16T09:00:00+01:00` — offset shown, seconds precision, no zone
/// name (parity with Temporal `toString({calendarName:'never',
/// smallestUnit:'second', timeZoneName:'never'})`).
pub fn format_instant(ms: i64, tz: Tz) -> String {
    chrono::DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| {
            dt.with_timezone(&tz)
                .format("%Y-%m-%dT%H:%M:%S%:z")
                .to_string()
        })
        .unwrap_or_default()
}

fn busy_status(ev: &ParsedEvent) -> &'static str {
    let Some(raw) = ev.busy_status_raw.as_deref() else {
        return "BUSY";
    };
    let upper = raw.to_ascii_uppercase();
    BUSY_STATUSES
        .iter()
        .find(|s| **s == upper)
        .copied()
        .unwrap_or("BUSY")
}

/// First Teams/Zoom/Meet link, searched in priority order: the Exchange
/// X-MICROSOFT-SKYPETEAMSMEETINGURL property (most reliable), then the RAW
/// description (the link usually sits inside the boilerplate that
/// clean_description strips), then the location.
pub fn extract_meeting_url(ev: &ParsedEvent) -> Option<String> {
    let sources = [
        ev.teams_url_prop.as_deref(),
        ev.description.as_deref(),
        ev.location.as_deref(),
    ];
    for source in sources.into_iter().flatten() {
        for pattern in MEETING_URL_PATTERNS.iter() {
            if let Some(m) = pattern.find(source) {
                return Some(
                    m.as_str()
                        .trim_end_matches(['>', ')', '.', ',', ';'])
                        .to_string(),
                );
            }
        }
    }
    None
}

/// Strip Teams/dial-in boilerplate (everything from the first underscore
/// separator), collapse blank runs, cap at ~2000 UTF-16 units (the TS cap
/// counts UTF-16 code units; this port never splits a scalar). The parser
/// already unescaped `\n` / `\,` and unfolded lines.
pub fn clean_description(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let mut text = raw.replace("\r\n", "\n");
    if let Some(m) = BOILERPLATE_START.find(&text) {
        text.truncate(m.start());
    }
    let text = BLANK_RUNS.replace_all(&text, "\n\n");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut units = 0usize;
    for (i, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if units + width > DESCRIPTION_MAX_UTF16_UNITS {
            return Some(format!("{}…", text[..i].trim_end()));
        }
        units += width;
    }
    Some(text.to_string())
}

pub fn to_api_event(inst: &ExpandedInstance<'_>, tz_default: Tz) -> ApiEvent {
    let ev = inst.event;
    let location = ev
        .location
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let (start, end) = match inst.time {
        InstanceTime::AllDay { start, end } => (
            start.format("%Y-%m-%d").to_string(),
            end.format("%Y-%m-%d").to_string(),
        ),
        InstanceTime::Timed { start_ms, end_ms } => (
            format_instant(start_ms, tz_default),
            format_instant(end_ms, tz_default),
        ),
    };
    ApiEvent {
        all_day: inst.is_all_day(),
        busy_status: busy_status(ev),
        description: clean_description(ev.description.as_deref()),
        end,
        is_recurring: inst.is_recurring,
        location,
        meeting_url: extract_meeting_url(ev),
        start,
        summary: ev.summary.clone().unwrap_or_default(),
    }
}
