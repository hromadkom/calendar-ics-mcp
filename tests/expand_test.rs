//! Port of tests/expand.spec.ts — every acceptance criterion keeps its named
//! block.

#![allow(clippy::unwrap_used)]

mod common;

use calendar_ics_mcp::ics::expand::{InstanceTime, day_window, expand_events, range_window};
use common::{PARSED, TZ, instance_of, instances_on, start_ms, uids_on, utc_ms};

mod ac1_rrule_expansion {
    use super::*;

    #[test]
    fn weekly_monday_lands_on_mondays_only() {
        assert!(uids_on("2026-03-02").contains(&"weekly-mon@fixture"));
        assert!(!uids_on("2026-03-03").contains(&"weekly-mon@fixture"));
        assert!(uids_on("2026-03-09").contains(&"weekly-mon@fixture"));
    }

    #[test]
    fn keeps_0900_local_wall_time_across_the_dst_change() {
        let before = instance_of("2026-03-23", "weekly-mon@fixture").unwrap();
        let after = instance_of("2026-03-30", "weekly-mon@fixture").unwrap();
        assert!(!before.is_all_day());
        assert_eq!(start_ms(&before), utc_ms("2026-03-23T08:00:00Z"));
        assert_eq!(start_ms(&after), utc_ms("2026-03-30T07:00:00Z"));
    }

    #[test]
    fn biweekly_keeps_its_phase() {
        assert!(uids_on("2026-03-03").contains(&"biweekly-tue@fixture"));
        assert!(!uids_on("2026-03-10").contains(&"biweekly-tue@fixture"));
        assert!(uids_on("2026-03-17").contains(&"biweekly-tue@fixture"));
        assert!(uids_on("2026-03-31").contains(&"biweekly-tue@fixture"));
    }

    #[test]
    fn until_in_utc_boundary_occurrence_included_later_ones_not() {
        // UNTIL=20260414T080000Z == 2026-04-14 10:00 CEST, exactly the
        // occurrence instant.
        let boundary = instance_of("2026-04-14", "biweekly-tue@fixture").unwrap();
        assert_eq!(start_ms(&boundary), utc_ms("2026-04-14T08:00:00Z"));
        assert!(!uids_on("2026-04-28").contains(&"biweekly-tue@fixture"));
    }

    #[test]
    fn monthly_2nd_friday_lands_on_the_2nd_friday() {
        assert!(uids_on("2026-03-13").contains(&"monthly-2fr@fixture"));
        assert!(!uids_on("2026-03-06").contains(&"monthly-2fr@fixture"));
        assert!(uids_on("2026-04-10").contains(&"monthly-2fr@fixture"));
        assert!(uids_on("2026-07-10").contains(&"monthly-2fr@fixture"));
    }
}

mod ac2_exdate {
    use super::*;

    #[test]
    fn skips_excluded_instances_multi_value_tzid_qualified_folded() {
        assert!(uids_on("2026-03-02").contains(&"weekly-exdate@fixture"));
        assert!(!uids_on("2026-03-09").contains(&"weekly-exdate@fixture"));
        assert!(uids_on("2026-03-16").contains(&"weekly-exdate@fixture"));
        assert!(!uids_on("2026-03-23").contains(&"weekly-exdate@fixture"));
        assert!(!uids_on("2026-04-06").contains(&"weekly-exdate@fixture"));
        assert!(uids_on("2026-04-13").contains(&"weekly-exdate@fixture"));
    }
}

mod ac3_recurrence_id_overrides {
    use super::*;

    #[test]
    fn an_override_replaces_the_base_occurrence_on_the_same_day_exactly_once() {
        let instances: Vec<_> = instances_on("2026-03-18")
            .into_iter()
            .filter(|inst| inst.event.uid == "weekly-ovr@fixture")
            .collect();
        assert_eq!(instances.len(), 1);
        let inst = &instances[0];
        assert!(!inst.is_all_day());
        assert!(inst.is_recurring);
        // 15:00 CET, not the base 13:00.
        assert_eq!(start_ms(inst), utc_ms("2026-03-18T14:00:00Z"));
        assert_eq!(inst.event.summary.as_deref(), Some("Team sync (moved)"));
        assert_eq!(inst.event.location.as_deref(), Some("Zasedačka C"));
    }

    #[test]
    fn other_weeks_of_the_overridden_series_stay_untouched() {
        let normal = instance_of("2026-03-11", "weekly-ovr@fixture").unwrap();
        assert_eq!(start_ms(&normal), utc_ms("2026-03-11T12:00:00Z"));
        assert_eq!(normal.event.summary.as_deref(), Some("Team sync"));
    }

    #[test]
    fn an_override_moved_across_days_disappears_from_old_day_appears_once_on_new() {
        assert!(!uids_on("2026-03-25").contains(&"weekly-ovr-moved@fixture"));
        let moved: Vec<_> = instances_on("2026-03-26")
            .into_iter()
            .filter(|inst| inst.event.uid == "weekly-ovr-moved@fixture")
            .collect();
        assert_eq!(moved.len(), 1);
        assert_eq!(start_ms(&moved[0]), utc_ms("2026-03-26T08:00:00Z"));
        assert_eq!(
            moved[0].event.summary.as_deref(),
            Some("Product check-in (rescheduled)")
        );
    }

    #[test]
    fn never_returns_the_same_uid_and_slot_twice_across_a_week_window() {
        let win = range_window("2026-03-23", "2026-03-29", TZ).unwrap();
        let instances = expand_events(&PARSED, &win).unwrap();
        let moved: Vec<_> = instances
            .iter()
            .filter(|inst| inst.event.uid == "weekly-ovr-moved@fixture")
            .collect();
        // Only the moved Thursday instance.
        assert_eq!(moved.len(), 1);
    }
}

mod ac5_all_day_events {
    use super::*;

    #[test]
    fn a_one_day_all_day_event_appears_on_its_day_only() {
        assert!(uids_on("2026-03-16").contains(&"allday-1@fixture"));
        assert!(!uids_on("2026-03-17").contains(&"allday-1@fixture"));
        let inst = instance_of("2026-03-16", "allday-1@fixture").unwrap();
        match inst.time {
            InstanceTime::AllDay { start, end } => {
                assert_eq!(start.to_string(), "2026-03-16");
                assert_eq!(end.to_string(), "2026-03-17");
            }
            InstanceTime::Timed { .. } => panic!("expected all-day"),
        }
    }

    #[test]
    fn a_multi_day_all_day_event_appears_on_each_covered_day_not_the_exclusive_end() {
        assert!(uids_on("2026-03-16").contains(&"allday-multi@fixture"));
        assert!(uids_on("2026-03-17").contains(&"allday-multi@fixture"));
        assert!(uids_on("2026-03-18").contains(&"allday-multi@fixture"));
        assert!(!uids_on("2026-03-19").contains(&"allday-multi@fixture"));
    }
}

mod overlap_semantics {
    use super::*;

    #[test]
    fn a_midnight_spanning_event_appears_on_both_days() {
        assert!(uids_on("2026-03-20").contains(&"midnight@fixture"));
        assert!(uids_on("2026-03-21").contains(&"midnight@fixture"));
    }

    #[test]
    fn an_event_ending_exactly_at_midnight_belongs_to_the_previous_day_only() {
        assert!(uids_on("2026-03-20").contains(&"midnight-end@fixture"));
        assert!(!uids_on("2026-03-21").contains(&"midnight-end@fixture"));
    }
}

mod ac4_timezone_handling {
    use super::*;

    #[test]
    fn interprets_the_second_windows_tzid_name_correctly() {
        let inst = instance_of("2026-03-18", "cest-tzid@fixture").unwrap();
        // 16:00 CET.
        assert_eq!(start_ms(&inst), utc_ms("2026-03-18T15:00:00Z"));
    }

    #[test]
    fn interprets_an_unknown_tzid_in_tz_default_not_the_system_zone() {
        // Without normalization this would depend on the host zone.
        let inst = instance_of("2026-03-19", "unknown-tzid@fixture").unwrap();
        // 09:00 Prague.
        assert_eq!(start_ms(&inst), utc_ms("2026-03-19T08:00:00Z"));
    }
}

mod windows {
    use super::*;

    #[test]
    fn day_window_covers_exactly_one_local_day_23h_on_the_spring_dst_day() {
        let dst = day_window(Some("2026-03-29"), TZ, 0).unwrap();
        assert_eq!(dst.end_ms - dst.start_ms, 23 * 3_600_000);
        let normal = day_window(Some("2026-03-30"), TZ, 0).unwrap();
        assert_eq!(normal.end_ms - normal.start_ms, 24 * 3_600_000);
    }

    #[test]
    fn range_window_accepts_31_days_and_rejects_32() {
        assert!(range_window("2026-03-01", "2026-03-31", TZ).is_ok());
        let err = range_window("2026-03-01", "2026-04-01", TZ).unwrap_err();
        assert_eq!(err.code(), "INVALID_RANGE");
        assert!(err.message().contains("maximum is 31"));
    }

    #[test]
    fn rejects_from_after_to_and_invalid_calendar_dates() {
        let err = range_window("2026-03-02", "2026-03-01", TZ).unwrap_err();
        assert_eq!(err.code(), "INVALID_RANGE");
        let err = day_window(Some("2026-02-30"), TZ, 0).unwrap_err();
        assert_eq!(err.code(), "INVALID_RANGE");
        assert!(err.message().contains("valid calendar date"));
    }
}

mod result_ordering {
    use super::*;

    #[test]
    fn sorts_by_start_with_all_day_events_first() {
        let summaries: Vec<String> = instances_on("2026-03-16")
            .iter()
            .map(|inst| inst.event.summary.clone().unwrap_or_default())
            .collect();
        assert_eq!(
            summaries,
            [
                "Dovolená",                // all-day, tie with Školení broken by summary
                "Školení",                 //
                "Weekly standup",          // 09:00
                "Q3 review, part 1; plán", // 10:00 (AC6: unescaped \, and \;)
                "Architecture board",      // 11:00
            ]
        );
    }
}
