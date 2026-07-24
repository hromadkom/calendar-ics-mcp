//! Port of tests/server.spec.ts — end-to-end tool tests driving the pure
//! `handle_message` dispatch (the Rust counterpart of InMemoryTransport).

#![allow(clippy::unwrap_used)]

mod common;

use serde_json::{Value, json};

use calendar_ics_mcp::feed::{Clock, FeedClient, HttpErr, HttpOk, Transport};
use calendar_ics_mcp::mcp::McpServer;
use calendar_ics_mcp::server::ServerState;
use common::{FIXTURE, FIXTURE_TRUNCATED, test_config, utc_ms};

struct StaticTransport {
    status: u16,
    body: &'static str,
}

impl Transport for StaticTransport {
    fn get(&self, _url: &str) -> Result<HttpOk, HttpErr> {
        Ok(HttpOk {
            status: self.status,
            body: self.body.to_string(),
        })
    }
}

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

fn server_at(status: u16, body: &'static str, now_ms: i64) -> McpServer<ServerState> {
    let cfg = test_config();
    let feed = FeedClient::new(
        cfg.clone(),
        StaticTransport { status, body },
        FixedClock(now_ms),
    );
    McpServer {
        name: "calendar-ics-mcp",
        version: env!("CARGO_PKG_VERSION"),
        tools: ServerState::new(cfg, feed),
    }
}

fn server(body: &'static str) -> McpServer<ServerState> {
    server_at(200, body, 1_000_000)
}

fn call(srv: &mut McpServer<ServerState>, name: &str, args: Value) -> Value {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": args },
    });
    srv.handle_message(&msg.to_string()).unwrap()["result"].clone()
}

fn text_of(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

mod get_events {
    use super::*;

    #[test]
    fn returns_the_expected_events_for_an_override_day_sorted() {
        let mut srv = server(FIXTURE);
        let result = call(&mut srv, "get_events", json!({ "date": "2026-03-18" }));
        assert!(result.get("isError").is_none());
        let structured = &result["structuredContent"];
        assert_eq!(structured["date"], "2026-03-18");
        assert_eq!(structured["timezone"], "Europe/Prague");
        let summaries: Vec<&str> = structured["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["summary"].as_str().unwrap())
            .collect();
        assert_eq!(
            summaries,
            [
                "Dovolená",
                "Product check-in",
                "Team sync (moved)",
                "CEST name variant"
            ]
        );
        // content mirrors structuredContent as JSON text.
        let text: Value = serde_json::from_str(text_of(&result)).unwrap();
        assert_eq!(&text, structured);
    }

    #[test]
    fn defaults_to_today_in_tz_default() {
        let mut srv = server_at(200, FIXTURE, utc_ms("2026-03-18T10:00:00Z"));
        let result = call(&mut srv, "get_events", json!({}));
        assert_eq!(result["structuredContent"]["date"], "2026-03-18");
    }

    #[test]
    fn rejects_a_malformed_date_at_the_schema_level() {
        let mut srv = server(FIXTURE);
        let result = call(&mut srv, "get_events", json!({ "date": "18.3.2026" }));
        assert_eq!(result["isError"], true);
        assert!(text_of(&result).contains("YYYY-MM-DD"));
    }

    #[test]
    fn returns_is_error_for_a_well_formed_but_invalid_calendar_date() {
        let mut srv = server(FIXTURE);
        let result = call(&mut srv, "get_events", json!({ "date": "2026-02-30" }));
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn fails_loudly_on_a_truncated_feed_ac7() {
        let mut srv = server(FIXTURE_TRUNCATED);
        let result = call(&mut srv, "get_events", json!({ "date": "2026-03-18" }));
        assert_eq!(result["isError"], true);
        let text = text_of(&result);
        assert!(text.contains("truncated"));
        assert!(!text.contains("SECRETPATH123"));
    }

    #[test]
    fn reports_fetch_failures_without_leaking_the_url() {
        let mut srv = server_at(404, "gone", 1_000_000);
        let result = call(&mut srv, "get_events", json!({ "date": "2026-03-18" }));
        assert_eq!(result["isError"], true);
        let text = text_of(&result);
        assert!(text.contains("HTTP 404"));
        assert!(text.contains("https://feedhost.example.com/…"));
        assert!(!text.contains("SECRETPATH123"));
    }
}

mod get_events_range {
    use super::*;

    #[test]
    fn accepts_a_31_day_range() {
        let mut srv = server(FIXTURE);
        let result = call(
            &mut srv,
            "get_events_range",
            json!({ "from": "2026-03-01", "to": "2026-03-31" }),
        );
        assert!(result.get("isError").is_none());
        let structured = &result["structuredContent"];
        assert_eq!(structured["from"], "2026-03-01");
        assert_eq!(structured["to"], "2026-03-31");
        // 5 Mondays of weekly-mon; 9.3. and 23.3. of weekly-exdate excluded, etc.
        assert!(structured["events"].as_array().unwrap().len() > 10);
    }

    #[test]
    fn rejects_a_32_day_range_with_a_helpful_message() {
        let mut srv = server(FIXTURE);
        let result = call(
            &mut srv,
            "get_events_range",
            json!({ "from": "2026-03-01", "to": "2026-04-01" }),
        );
        assert_eq!(result["isError"], true);
        assert!(text_of(&result).contains("maximum is 31"));
    }

    #[test]
    fn rejects_from_after_to() {
        let mut srv = server(FIXTURE);
        let result = call(
            &mut srv,
            "get_events_range",
            json!({ "from": "2026-03-02", "to": "2026-03-01" }),
        );
        assert_eq!(result["isError"], true);
    }
}

mod feed_info {
    use super::*;

    #[test]
    fn reports_size_count_range_and_integrity_for_a_healthy_feed() {
        let mut srv = server(FIXTURE);
        let result = call(&mut srv, "feed_info", json!({}));
        assert!(result.get("isError").is_none());
        let info = &result["structuredContent"];
        assert_eq!(info["feed_bytes"], FIXTURE.len());
        assert_eq!(info["vevent_count"], 18);
        assert_eq!(info["ends_with_end_vcalendar"], true);
        assert_eq!(info["source_host"], "https://feedhost.example.com/…");
        // monthly-2fr series start.
        assert_eq!(info["dtstart_min"], "2026-01-09T13:00:00.000Z");
        assert!(info["last_fetch_at"].is_string());
        assert!(!result.to_string().contains("SECRETPATH123"));
    }

    #[test]
    fn still_answers_for_a_truncated_feed_and_reports_the_problem() {
        let mut srv = server(FIXTURE_TRUNCATED);
        let result = call(&mut srv, "feed_info", json!({}));
        assert!(result.get("isError").is_none());
        let info = &result["structuredContent"];
        assert_eq!(info["ends_with_end_vcalendar"], false);
        assert_eq!(info["feed_bytes"], FIXTURE_TRUNCATED.len());
        assert_eq!(info["dtstart_min"], Value::Null);
        assert_eq!(info["dtstart_max"], Value::Null);
    }
}

mod tool_listing {
    use super::*;

    #[test]
    fn exposes_exactly_the_three_read_only_tools() {
        let mut srv = server(FIXTURE);
        let res = srv
            .handle_message(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#)
            .unwrap();
        let tools = res["result"]["tools"].as_array().unwrap();
        let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        names.sort_unstable();
        assert_eq!(names, ["feed_info", "get_events", "get_events_range"]);
        for tool in tools {
            assert_eq!(tool["annotations"]["destructiveHint"], false);
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
        }
    }
}
