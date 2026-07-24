//! Port of tests/format.spec.ts.

#![allow(clippy::unwrap_used)]

mod common;

use calendar_ics_mcp::ics::expand::{ExpandedInstance, InstanceTime};
use calendar_ics_mcp::ics::format::{
    ApiEvent, clean_description, extract_meeting_url, to_api_event,
};
use calendar_ics_mcp::ics::model::{IcsDateTime, ParsedEvent};
use common::{TZ, instance_of, utc_ms};

fn api(date: &str, uid: &str) -> ApiEvent {
    let inst = instance_of(date, uid).unwrap_or_else(|| panic!("no instance of {uid} on {date}"));
    to_api_event(&inst, TZ)
}

/// Minimal synthetic event for helper-level tests (the TS suite's
/// `fakeEvent`).
fn fake_event() -> ParsedEvent {
    ParsedEvent {
        uid: "fake@test".to_string(),
        summary: Some("x".to_string()),
        description: None,
        location: None,
        busy_status_raw: None,
        teams_url_prop: None,
        dtstart: IcsDateTime::Utc(
            chrono::DateTime::from_timestamp_millis(utc_ms("2026-03-20T22:30:00Z"))
                .unwrap()
                .naive_utc(),
        ),
        dtend: None,
        rrule_raw: None,
        exdates: Vec::new(),
        recurrence_id: None,
        sequence: 0,
    }
}

fn instance_over(ev: &ParsedEvent) -> ExpandedInstance<'_> {
    ExpandedInstance {
        event: ev,
        time: InstanceTime::Timed {
            start_ms: utc_ms("2026-03-20T22:30:00Z"),
            end_ms: utc_ms("2026-03-20T23:30:00Z"),
        },
        is_recurring: false,
    }
}

mod timed_formatting {
    use super::*;

    #[test]
    fn renders_the_tz_default_offset_plus1_in_winter_plus2_in_summer() {
        let winter = api("2026-03-23", "weekly-mon@fixture");
        assert!(!winter.all_day);
        assert_eq!(winter.start, "2026-03-23T09:00:00+01:00");
        assert_eq!(winter.end, "2026-03-23T09:30:00+01:00");
        let summer = api("2026-03-30", "weekly-mon@fixture");
        assert_eq!(summer.start, "2026-03-30T09:00:00+02:00");
        assert_eq!(summer.end, "2026-03-30T09:30:00+02:00");
    }
}

mod all_day_formatting {
    use super::*;

    #[test]
    fn renders_date_only_with_the_exclusive_end_date() {
        let event = api("2026-03-16", "allday-multi@fixture");
        assert!(event.all_day);
        assert_eq!(event.busy_status, "OOF");
        assert_eq!(event.start, "2026-03-16");
        assert_eq!(event.end, "2026-03-19");
        assert_eq!(event.summary, "Dovolená");
    }
}

mod ac6_escaping_and_folding {
    use super::*;

    #[test]
    fn unescapes_commas_semicolons_and_reassembles_folded_lines() {
        let event = api("2026-03-16", "escape@fixture");
        assert_eq!(event.summary, "Q3 review, part 1; plán");
        let description = event.description.unwrap();
        assert!(description.contains("line one\nline two, with a comma"));
        assert!(description.contains("mid-word to prove"));
        assert!(description.contains("diakritika ěščřžýáíé"));
        assert!(!description.contains('\\'));
    }
}

mod busy_status {
    use super::*;

    #[test]
    fn maps_all_four_exchange_values() {
        assert_eq!(api("2026-03-23", "weekly-mon@fixture").busy_status, "BUSY");
        assert_eq!(api("2026-03-17", "teams@fixture").busy_status, "TENTATIVE");
        assert_eq!(api("2026-03-16", "escape@fixture").busy_status, "FREE");
        assert_eq!(api("2026-03-16", "allday-1@fixture").busy_status, "OOF");
    }

    #[test]
    fn the_override_carries_its_own_busy_status() {
        assert_eq!(
            api("2026-03-18", "weekly-ovr@fixture").busy_status,
            "TENTATIVE"
        );
    }

    #[test]
    fn defaults_to_busy_when_the_property_is_missing_or_unknown() {
        let bare = fake_event();
        assert_eq!(to_api_event(&instance_over(&bare), TZ).busy_status, "BUSY");

        let mut weird = fake_event();
        weird.busy_status_raw = Some("WORKINGELSEWHERE".to_string());
        assert_eq!(to_api_event(&instance_over(&weird), TZ).busy_status, "BUSY");
    }
}

mod meeting_url_extraction {
    use super::*;

    #[test]
    fn finds_the_teams_link_inside_the_description_boilerplate() {
        let event = api("2026-03-17", "teams@fixture");
        assert!(event.meeting_url.unwrap().starts_with(
            "https://teams.microsoft.com/l/meetup-join/19%3ameeting_ZmVlZDESCRIPTION"
        ));
    }

    #[test]
    fn prefers_the_x_prop_over_the_description() {
        let event = api("2026-03-19", "teams-xprop@fixture");
        let url = event.meeting_url.unwrap();
        assert!(url.contains("meeting_WFBSTVBYUFJPUA"));
        assert!(!url.contains("REVDT1lERUNPWQ"));
    }

    #[test]
    fn falls_back_to_a_zoom_link_in_the_location() {
        let event = api("2026-03-17", "zoom-loc@fixture");
        assert_eq!(
            event.meeting_url.as_deref(),
            Some("https://example.zoom.us/j/2718281828?pwd=Zm9vYmFy")
        );
    }

    #[test]
    fn returns_null_when_there_is_no_link() {
        assert_eq!(api("2026-03-23", "weekly-mon@fixture").meeting_url, None);
    }

    #[test]
    fn trims_trailing_punctuation_and_supports_google_meet() {
        let mut ev = fake_event();
        ev.description = Some("Join: https://meet.google.com/abc-defg-hij.".to_string());
        assert_eq!(
            extract_meeting_url(&ev).as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
    }
}

mod clean_description_tests {
    use super::*;

    #[test]
    fn strips_everything_from_the_teams_underscore_separator_on() {
        let event = api("2026-03-17", "teams@fixture");
        assert_eq!(
            event.description.as_deref(),
            Some("Agenda: výsledky Q1 a plán Q2")
        );
    }

    #[test]
    fn returns_null_for_missing_or_boilerplate_only_descriptions() {
        assert_eq!(clean_description(None), None);
        assert_eq!(
            clean_description(Some("________________\nMicrosoft Teams meeting")),
            None
        );
        assert_eq!(clean_description(Some("   \n ")), None);
    }

    #[test]
    fn caps_at_2000_characters_with_an_ellipsis() {
        let cleaned = clean_description(Some(&"x".repeat(3000))).unwrap();
        assert_eq!(cleaned.chars().count(), 2001);
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn collapses_runs_of_blank_lines() {
        assert_eq!(
            clean_description(Some("a\n\n\n\n\nb")).as_deref(),
            Some("a\n\nb")
        );
    }
}

mod location {
    use super::*;

    #[test]
    fn passes_the_location_through_and_nulls_empty_ones() {
        assert_eq!(
            api("2026-03-23", "weekly-mon@fixture").location.as_deref(),
            Some("Zasedačka A")
        );
        assert_eq!(
            api("2026-03-18", "weekly-ovr@fixture").location.as_deref(),
            Some("Zasedačka C")
        );
        assert_eq!(api("2026-03-16", "allday-1@fixture").location, None);
    }
}
