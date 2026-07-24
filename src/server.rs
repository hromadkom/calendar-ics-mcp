//! Tool registry: declarations (schemas, annotations), argument validation,
//! result shaping, and the tool handlers themselves.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::config::Config;
use crate::errors::{AppError, mask_ics_url, sanitize_text};
use crate::feed::FeedClient;
use crate::ics::expand::{EventWindow, day_window, expand_events, instant_ms, range_window};
use crate::ics::format::{ApiEvent, to_api_event};
use crate::ics::model::ParsedCalendar;
use crate::ics::parse::parse_calendar;
use crate::ics::tzids::{normalize_tzids, unfold_ics};
use crate::logger;
use crate::mcp::{RpcError, ToolProvider};

pub struct ServerState {
    pub cfg: Config,
    feed: FeedClient,
    tools: Value,
    warned_tzids: HashSet<String>,
}

impl ServerState {
    pub fn new(cfg: Config, feed: FeedClient) -> Self {
        let tools = build_tools(cfg.tz_default.name());
        Self {
            cfg,
            feed,
            tools,
            warned_tzids: HashSet::new(),
        }
    }

    /// TZID-normalize and warn once per newly-seen unknown name (the warning
    /// half of the TS `parseFeed`).
    fn normalize_and_warn(&mut self, unfolded: &str) -> String {
        let normalized = normalize_tzids(unfolded, self.cfg.tz_default);
        for name in normalized.unknown {
            if self.warned_tzids.insert(name.clone()) {
                logger::warn(
                    "Unknown TZID in feed; interpreting it as TZ_DEFAULT",
                    &[
                        ("fallback", json!(self.cfg.tz_default.name())),
                        ("tzid", json!(name)),
                    ],
                );
            }
        }
        normalized.ics
    }

    /// All events overlapping the window, shaped for output. `Err` carries a
    /// ready-to-return failure result.
    fn events_for_window(&mut self, win: &EventWindow) -> Result<Vec<ApiEvent>, Value> {
        let unfolded = match self.feed.get_validated_raw() {
            Ok(raw) => unfold_ics(raw),
            Err(err) => return Err(failure(&err, &self.cfg.ics_url)),
        };
        let ics = self.normalize_and_warn(&unfolded);
        let cal: ParsedCalendar = parse_calendar(&ics, self.cfg.tz_default)
            .map_err(|e| self.internal(&format!("ParseError: {e}")))?;
        let instances =
            expand_events(&cal, win).map_err(|e| self.internal(&format!("SlotError: {e}")))?;
        Ok(instances
            .iter()
            .map(|inst| to_api_event(inst, self.cfg.tz_default))
            .collect())
    }

    /// Unexpected (non-domain) failure: log the sanitized detail, return the
    /// fixed internal-error text (the TS `failure()` fallback branch).
    fn internal(&self, detail: &str) -> Value {
        logger::error(
            "Tool failed with an unexpected error",
            &[("detail", json!(sanitize_text(detail, &self.cfg.ics_url)))],
        );
        error_result(
            "Internal error while processing the calendar feed; details are on stderr \
             (Claude Desktop MCP log)."
                .to_string(),
        )
    }

    fn get_events(&mut self, args: &Value) -> Value {
        let date = match date_arg(args, "date", false) {
            Err(detail) => return invalid_args("get_events", &detail),
            Ok(v) => v,
        };
        let win = match day_window(date.as_deref(), self.cfg.tz_default, self.feed.now_ms()) {
            Ok(win) => win,
            Err(err) => return failure(&err, &self.cfg.ics_url),
        };
        match self.events_for_window(&win) {
            Ok(events) => success(json!({
                "date": win.start_date.format("%Y-%m-%d").to_string(),
                "events": events,
                "timezone": self.cfg.tz_default.name(),
            })),
            Err(result) => result,
        }
    }

    fn get_events_range(&mut self, args: &Value) -> Value {
        let from = match date_arg(args, "from", true) {
            Err(detail) => return invalid_args("get_events_range", &detail),
            Ok(v) => v,
        };
        let to = match date_arg(args, "to", true) {
            Err(detail) => return invalid_args("get_events_range", &detail),
            Ok(v) => v,
        };
        let (Some(from), Some(to)) = (from, to) else {
            return invalid_args("get_events_range", "missing required arguments");
        };
        let win = match range_window(&from, &to, self.cfg.tz_default) {
            Ok(win) => win,
            Err(err) => return failure(&err, &self.cfg.ics_url),
        };
        match self.events_for_window(&win) {
            Ok(events) => success(json!({
                "events": events,
                "from": from,
                "timezone": self.cfg.tz_default.name(),
                "to": to,
            })),
            Err(result) => result,
        }
    }

    fn feed_info(&mut self) -> Value {
        let (bytes, ends_valid, fetched_at_ms, vevent_count, unfolded) = {
            let snapshot = match self.feed.get_snapshot() {
                Ok(s) => s,
                Err(err) => return failure(&err, &self.cfg.ics_url),
            };
            (
                snapshot.bytes,
                snapshot.ends_valid,
                snapshot.fetched_at_ms,
                count_vevents(&snapshot.raw),
                unfold_ics(&snapshot.raw),
            )
        };
        let cache_age_seconds =
            ((self.feed.now_ms() - fetched_at_ms) as f64 / 1000.0).round() as i64;

        // DTSTART range over masters AND overrides. Unparseable (e.g.
        // truncated) feed — a null range IS the diagnosis.
        let ics = self.normalize_and_warn(&unfolded);
        let (dtstart_min, dtstart_max) = match parse_calendar(&ics, self.cfg.tz_default) {
            Err(_) => (Value::Null, Value::Null),
            Ok(cal) => {
                let starts: Vec<i64> = cal
                    .groups
                    .iter()
                    .flat_map(|g| g.master.iter().chain(g.overrides.iter()))
                    .map(|ev| instant_ms(&ev.dtstart))
                    .collect();
                match (starts.iter().min(), starts.iter().max()) {
                    (Some(min), Some(max)) => {
                        (json!(iso_utc_millis(*min)), json!(iso_utc_millis(*max)))
                    }
                    _ => (Value::Null, Value::Null),
                }
            }
        };

        success(json!({
            "cache_age_seconds": cache_age_seconds,
            "dtstart_max": dtstart_max,
            "dtstart_min": dtstart_min,
            "ends_with_end_vcalendar": ends_valid,
            "feed_bytes": bytes,
            "last_fetch_at": iso_utc_millis(fetched_at_ms),
            "source_host": mask_ics_url(&self.cfg.ics_url),
            "vevent_count": vevent_count,
        }))
    }
}

/// Port of `(raw.match(/^BEGIN:VEVENT/gm) ?? []).length` — counts every line
/// beginning with BEGIN:VEVENT, so it works on truncated feeds and counts
/// override VEVENTs too.
fn count_vevents(raw: &str) -> usize {
    raw.lines()
        .filter(|l| l.starts_with("BEGIN:VEVENT"))
        .count()
}

/// UTC with milliseconds — the `Date.toISOString()` shape.
fn iso_utc_millis(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

/// Success result: `content[0].text` mirrors `structuredContent` as its exact
/// JSON serialization, like the TS `success()` helper.
fn success(structured: Value) -> Value {
    let text = structured.to_string();
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": structured })
}

/// Domain failure: sanitized message in an `isError` result — never a
/// protocol error across the MCP boundary.
fn failure(err: &AppError, ics_url: &str) -> Value {
    error_result(sanitize_text(&err.message(), ics_url))
}

impl ToolProvider for ServerState {
    fn list_tools(&self) -> Value {
        self.tools.clone()
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, RpcError> {
        match name {
            "feed_info" => Ok(self.feed_info()),
            "get_events" => Ok(self.get_events(args)),
            "get_events_range" => Ok(self.get_events_range(args)),
            _ => Err(RpcError {
                code: -32602,
                message: format!("Unknown tool: {name}"),
            }),
        }
    }
}

/// Validate a YYYY-MM-DD-shaped argument (format only — real calendar dates
/// are checked later by the window builders, exactly like the TS zod regex).
fn date_arg(args: &Value, field: &str, required: bool) -> Result<Option<String>, String> {
    match args.get(field) {
        None => {
            if required {
                Err(format!("{field} expected a date in YYYY-MM-DD format"))
            } else {
                Ok(None)
            }
        }
        Some(Value::String(s)) if is_iso_date_shape(s) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{field} expected a date in YYYY-MM-DD format")),
    }
}

fn is_iso_date_shape(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0usize, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
}

/// Schema-violation shape mirroring the TS SDK: an `isError` tool result,
/// never a protocol error.
fn invalid_args(tool: &str, detail: &str) -> Value {
    error_result(format!("Invalid arguments for tool {tool}: {detail}"))
}

fn error_result(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

fn build_tools(tz: &str) -> Value {
    let annotations = json!({
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": true,
        "readOnlyHint": true,
    });
    let nullable_string = json!({ "type": ["string", "null"] });
    let api_event_schema = json!({
        "type": "object",
        "properties": {
            "all_day": { "type": "boolean" },
            "busy_status": { "type": "string", "enum": ["BUSY", "TENTATIVE", "FREE", "OOF"] },
            "description": nullable_string,
            "end": { "type": "string" },
            "is_recurring": { "type": "boolean" },
            "location": nullable_string,
            "meeting_url": nullable_string,
            "start": { "type": "string" },
            "summary": { "type": "string" },
        },
        "required": [
            "all_day", "busy_status", "description", "end", "is_recurring",
            "location", "meeting_url", "start", "summary",
        ],
        "additionalProperties": false,
    });
    let events_array = json!({ "type": "array", "items": api_event_schema });
    let date_pattern = "^\\d{4}-\\d{2}-\\d{2}$";

    json!([
        {
            "name": "feed_info",
            "title": "Feed diagnostics",
            "description": "Diagnostics for the ICS feed download: size in bytes, VEVENT count, \
                DTSTART range, last fetch time, cache age, and whether the payload ended with \
                END:VCALENDAR (i.e. arrived complete)",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "cache_age_seconds": { "type": "number" },
                    "dtstart_max": nullable_string,
                    "dtstart_min": nullable_string,
                    "ends_with_end_vcalendar": { "type": "boolean" },
                    "feed_bytes": { "type": "number" },
                    "last_fetch_at": { "type": "string" },
                    "source_host": { "type": "string" },
                    "vevent_count": { "type": "number" },
                },
                "required": [
                    "cache_age_seconds", "dtstart_max", "dtstart_min", "ends_with_end_vcalendar",
                    "feed_bytes", "last_fetch_at", "source_host", "vevent_count",
                ],
                "additionalProperties": false,
            },
            "annotations": annotations,
        },
        {
            "name": "get_events",
            "title": "Get events for a day",
            "description": format!(
                "List calendar events that overlap the given local day, sorted by start time. \
                 Times are ISO 8601 with the {tz} offset; all-day events use date-only strings \
                 with an exclusive end date."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "date": {
                        "type": "string",
                        "pattern": date_pattern,
                        "description": format!("Day to list (YYYY-MM-DD). Defaults to today in {tz}."),
                    },
                },
                "additionalProperties": false,
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "date": { "type": "string" },
                    "timezone": { "type": "string" },
                    "events": events_array,
                },
                "required": ["date", "timezone", "events"],
                "additionalProperties": false,
            },
            "annotations": annotations,
        },
        {
            "name": "get_events_range",
            "title": "Get events for a date range",
            "description": "List calendar events overlapping an inclusive date range (max 31 \
                days), sorted by start time. Same output format as get_events.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": {
                        "type": "string",
                        "pattern": date_pattern,
                        "description": "First day of the range (YYYY-MM-DD, inclusive).",
                    },
                    "to": {
                        "type": "string",
                        "pattern": date_pattern,
                        "description": "Last day of the range (YYYY-MM-DD, inclusive).",
                    },
                },
                "required": ["from", "to"],
                "additionalProperties": false,
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string" },
                    "timezone": { "type": "string" },
                    "to": { "type": "string" },
                    "events": events_array,
                },
                "required": ["from", "timezone", "to", "events"],
                "additionalProperties": false,
            },
            "annotations": annotations,
        },
    ])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::load_config;
    use crate::feed::{Clock, HttpErr, HttpOk, Transport};

    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/feed.ics"
    ));
    const FIXTURE_TRUNCATED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/feed-truncated.ics"
    ));

    struct FixtureTransport(&'static str);

    impl Transport for FixtureTransport {
        fn get(&self, _url: &str) -> Result<HttpOk, HttpErr> {
            Ok(HttpOk {
                status: 200,
                body: self.0.to_string(),
            })
        }
    }

    struct FixedClock;

    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            1_000_000
        }
    }

    fn state_with(feed_body: &'static str) -> ServerState {
        let cfg = load_config(|k| match k {
            "ICS_URL" => Some(
                "https://feedhost.example.com/owa/calendar/SECRETPATH123/reachcalendar.ics"
                    .to_string(),
            ),
            _ => None,
        })
        .unwrap();
        let feed = FeedClient::new(cfg.clone(), FixtureTransport(feed_body), FixedClock);
        ServerState::new(cfg, feed)
    }

    fn state() -> ServerState {
        state_with(FIXTURE)
    }

    #[test]
    fn feed_info_reports_a_healthy_feed_without_leaking_the_secret() {
        let mut s = state();
        let res = s.call_tool("feed_info", &json!({})).unwrap();
        let structured = &res["structuredContent"];
        assert_eq!(structured["ends_with_end_vcalendar"], true);
        assert_eq!(structured["feed_bytes"], FIXTURE.len());
        assert_eq!(structured["vevent_count"], 18);
        assert_eq!(structured["cache_age_seconds"], 0);
        assert_eq!(structured["source_host"], "https://feedhost.example.com/…");
        // content[0].text is the exact JSON of structuredContent.
        let text: Value =
            serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(&text, structured);
        assert!(!res.to_string().contains("SECRETPATH123"));
    }

    #[test]
    fn feed_info_still_answers_on_a_truncated_feed() {
        let mut s = state_with(FIXTURE_TRUNCATED);
        let res = s.call_tool("feed_info", &json!({})).unwrap();
        assert!(res.get("isError").is_none());
        let structured = &res["structuredContent"];
        assert_eq!(structured["ends_with_end_vcalendar"], false);
        assert_eq!(structured["feed_bytes"], FIXTURE_TRUNCATED.len());
    }

    #[test]
    fn lists_exactly_the_three_tools_with_read_only_annotations() {
        let tools = state().list_tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["feed_info", "get_events", "get_events_range"]);
        for tool in tools.as_array().unwrap() {
            assert_eq!(tool["annotations"]["destructiveHint"], false);
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn descriptions_interpolate_the_configured_timezone() {
        let tools = state().list_tools();
        let get_events = &tools.as_array().unwrap()[1];
        assert!(
            get_events["description"]
                .as_str()
                .unwrap()
                .contains("Europe/Prague")
        );
        assert!(
            get_events["inputSchema"]["properties"]["date"]["description"]
                .as_str()
                .unwrap()
                .contains("Europe/Prague")
        );
    }

    #[test]
    fn malformed_date_arguments_become_is_error_results() {
        let mut s = state();
        let res = s
            .call_tool("get_events", &json!({ "date": "18-03-2026" }))
            .unwrap();
        assert_eq!(res["isError"], true);
        assert!(
            res["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("YYYY-MM-DD")
        );

        let res = s
            .call_tool("get_events_range", &json!({ "from": "2026-03-01" }))
            .unwrap();
        assert_eq!(res["isError"], true);
        assert!(res["content"][0]["text"].as_str().unwrap().contains("to "));
    }

    #[test]
    fn unknown_tools_are_protocol_errors() {
        let err = state().call_tool("bogus", &json!({})).unwrap_err();
        assert_eq!(err.code, -32602);
    }
}
