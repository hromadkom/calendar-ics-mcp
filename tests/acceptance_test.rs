//! Port of tests/acceptance.spec.ts: `get_events` for known dates must return
//! the exact hand-computed list (the fixture mirrors real Exchange feed
//! shapes). The final acceptance — diffing against Outlook web on the live
//! feed — is a manual step documented in AGENTS.md.

#![allow(clippy::unwrap_used)]

mod common;

use serde_json::{Value, json};

use calendar_ics_mcp::ics::format::to_api_event;
use common::{TZ, instances_on};

fn api_events_on(date: &str) -> Vec<Value> {
    instances_on(date)
        .iter()
        .map(|inst| serde_json::to_value(to_api_event(inst, TZ)).unwrap())
        .collect()
}

/// `toMatchObject` semantics: every key in `expected` must match the actual
/// value recursively; arrays must have equal length and match elementwise.
fn assert_matches_subset(actual: &Value, expected: &Value, path: &str) {
    match expected {
        Value::Object(exp) => {
            let act = actual
                .as_object()
                .unwrap_or_else(|| panic!("{path}: expected object, got {actual}"));
            for (key, exp_value) in exp {
                let act_value = act
                    .get(key)
                    .unwrap_or_else(|| panic!("{path}.{key}: missing (actual: {actual})"));
                assert_matches_subset(act_value, exp_value, &format!("{path}.{key}"));
            }
        }
        Value::Array(exp) => {
            let act = actual
                .as_array()
                .unwrap_or_else(|| panic!("{path}: expected array, got {actual}"));
            assert_eq!(
                act.len(),
                exp.len(),
                "{path}: length mismatch (actual: {actual:#})"
            );
            for (i, (a, e)) in act.iter().zip(exp).enumerate() {
                assert_matches_subset(a, e, &format!("{path}[{i}]"));
            }
        }
        other => assert_eq!(actual, other, "{path}: value mismatch"),
    }
}

fn assert_day(date: &str, expected: Value) {
    let actual = Value::Array(api_events_on(date));
    assert_matches_subset(&actual, &expected, date);
}

#[test]
fn monday_2026_03_16_two_all_day_events_plus_three_timed_series_instances() {
    assert_day(
        "2026-03-16",
        json!([
            {
                "all_day": true,
                "busy_status": "OOF",
                "end": "2026-03-19",
                "start": "2026-03-16",
                "summary": "Dovolená",
            },
            {
                "all_day": true,
                "busy_status": "OOF",
                "end": "2026-03-17",
                "start": "2026-03-16",
                "summary": "Školení",
            },
            {
                "all_day": false,
                "is_recurring": true,
                "location": "Zasedačka A",
                "start": "2026-03-16T09:00:00+01:00",
                "summary": "Weekly standup",
            },
            { "start": "2026-03-16T10:00:00+01:00", "summary": "Q3 review, part 1; plán" },
            { "is_recurring": true, "start": "2026-03-16T11:00:00+01:00", "summary": "Architecture board" },
        ]),
    );
}

#[test]
fn wednesday_2026_03_18_the_moved_override_replaces_the_base_occurrence() {
    assert_day(
        "2026-03-18",
        json!([
            { "all_day": true, "summary": "Dovolená" },
            { "start": "2026-03-18T14:00:00+01:00", "summary": "Product check-in" },
            {
                "busy_status": "TENTATIVE",
                "is_recurring": true,
                "location": "Zasedačka C",
                "start": "2026-03-18T15:00:00+01:00",
                "summary": "Team sync (moved)",
            },
            { "start": "2026-03-18T16:00:00+01:00", "summary": "CEST name variant" },
        ]),
    );
}

#[test]
fn monday_2026_03_30_after_the_dst_change_times_keep_local_wall_clock_at_plus2() {
    assert_day(
        "2026-03-30",
        json!([
            { "start": "2026-03-30T09:00:00+02:00", "summary": "Weekly standup" },
            { "start": "2026-03-30T11:00:00+02:00", "summary": "Architecture board" },
        ]),
    );
}

#[test]
fn tuesday_2026_03_17_teams_zoom_biweekly_with_meeting_urls_extracted() {
    assert_day(
        "2026-03-17",
        json!([
            { "all_day": true, "summary": "Dovolená" },
            {
                "busy_status": "TENTATIVE",
                "description": "Agenda: výsledky Q1 a plán Q2",
                "start": "2026-03-17T09:00:00+01:00",
                "summary": "Čtvrtletní review",
            },
            { "start": "2026-03-17T10:00:00+01:00", "summary": "Biweekly sync" },
            {
                "meeting_url": "https://example.zoom.us/j/2718281828?pwd=Zm9vYmFy",
                "start": "2026-03-17T11:00:00+01:00",
                "summary": "External call",
            },
        ]),
    );
}
