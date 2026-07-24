//! RRULE slot generation via the `rrule` crate, used strictly as an
//! occurrence generator: EXDATE/RDATE are never handed to it — all exclusion
//! and override logic lives in `expand.rs`, exactly like the TS layer kept it
//! on top of rrule.js.

use chrono::{Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

use crate::ics::expand::resolve_local;
use crate::ics::model::IcsDateTime;

/// Safety cap against hostile rules (e.g. FREQ=SECONDLY) — the TS stack had
/// rrule.js-internal limits; hitting this is an error, never a hang.
const MAX_ITERATIONS: usize = 100_000;

#[derive(Debug)]
pub struct SlotError(pub String);

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SlotError {}

/// A Z-less UNTIL is parsed by the rrule crate in the MACHINE timezone (and
/// then rejected for tz-aware starts). node-ical 0.26 resolves it in the
/// EVENT's timezone (verified empirically: `UNTIL=20260414T080000` with a
/// Europe/Berlin DTSTART becomes 06:00Z, on any host) — reproduce that:
/// resolve the wall time in DTSTART's zone, then rewrite as UTC before the
/// crate sees it. UTC/floating/all-day starts already live in the UTC domain.
fn ensure_until_utc(rrule_raw: &str, dtstart: &IcsDateTime) -> String {
    rrule_raw
        .split(';')
        .map(|part| match part.split_once('=') {
            Some((k, v)) if k.eq_ignore_ascii_case("UNTIL") && !v.ends_with(['Z', 'z']) => {
                format!("{k}={}", until_to_utc(v, dtstart))
            }
            _ => part.to_string(),
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn until_to_utc(value: &str, dtstart: &IcsDateTime) -> String {
    let naive: Option<NaiveDateTime> = if value.contains('T') {
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()
    } else {
        // Date-only UNTIL means END of that day — 23:59:59 wall time in the
        // event's zone (node-ical parity, verified empirically) — so the
        // occurrence on the UNTIL date itself is still included.
        NaiveDate::parse_from_str(value, "%Y%m%d")
            .ok()
            .map(|d| d.and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap_or(NaiveTime::MIN)))
    };
    let Some(naive) = naive else {
        // Unparseable — hand it to the crate unchanged and let it reject.
        return value.to_string();
    };
    let utc = match dtstart {
        IcsDateTime::Zoned { tz, .. } => resolve_local(*tz, naive).with_timezone(&Utc).naive_utc(),
        _ => naive,
    };
    utc.format("%Y%m%dT%H%M%SZ").to_string()
}

/// DTSTART for the rrule crate: zoned wall time generates in its own zone
/// (wall-clock-preserving across DST); UTC/floating instants and all-day
/// dates live in the UTC domain.
fn dtstart_for_rrule(dtstart: &IcsDateTime) -> chrono::DateTime<rrule::Tz> {
    match dtstart {
        IcsDateTime::Date(d) => rrule::Tz::UTC.from_utc_datetime(&d.and_time(NaiveTime::MIN)),
        IcsDateTime::Utc(n) | IcsDateTime::Floating(n) => rrule::Tz::UTC.from_utc_datetime(n),
        IcsDateTime::Zoned { local, tz } => {
            let tz: rrule::Tz = (*tz).into();
            match tz.from_local_datetime(local) {
                LocalResult::Single(dt) => dt,
                LocalResult::Ambiguous(earlier, _) => earlier,
                // DTSTART inside a DST gap — pathological; shift forward like
                // Temporal 'compatible'.
                LocalResult::None => tz
                    .from_local_datetime(&(*local + Duration::hours(1)))
                    .earliest()
                    .unwrap_or_else(|| tz.from_utc_datetime(local)),
            }
        }
    }
}

/// Occurrence instants (epoch ms) in `[lower_ms, upper_ms]`, BOTH inclusive —
/// reproducing TS `rrule.between(lower, upper, inclusive=true)` via the raw
/// iterator (monotonically increasing, no crate-side windowing opinions).
pub fn series_slots(
    dtstart: &IcsDateTime,
    rrule_raw: &str,
    lower_ms: i64,
    upper_ms: i64,
) -> Result<Vec<i64>, SlotError> {
    let rule_value = ensure_until_utc(rrule_raw, dtstart);
    let unvalidated: rrule::RRule<rrule::Unvalidated> = rule_value
        .parse()
        .map_err(|e| SlotError(format!("invalid RRULE: {e}")))?;
    let dt_start = dtstart_for_rrule(dtstart);
    let validated = unvalidated
        .validate(dt_start)
        .map_err(|e| SlotError(format!("invalid RRULE: {e}")))?;
    let set = rrule::RRuleSet::new(dt_start).rrule(validated);

    let mut slots = Vec::new();
    for (i, occurrence) in set.into_iter().enumerate() {
        if i >= MAX_ITERATIONS {
            return Err(SlotError(format!(
                "RRULE expansion exceeded {MAX_ITERATIONS} iterations"
            )));
        }
        let ms = occurrence.timestamp_millis();
        if ms < lower_ms {
            continue;
        }
        if ms > upper_ms {
            break;
        }
        slots.push(ms);
    }
    Ok(slots)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime};
    use chrono_tz::Tz;

    use super::*;

    fn zoned(y: i32, mo: u32, d: u32, h: u32, mi: u32, tz: Tz) -> IcsDateTime {
        IcsDateTime::Zoned {
            local: local(y, mo, d, h, mi),
            tz,
        }
    }

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn utc_ms(iso: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis()
    }

    /// The fixture's weekly Monday 09:00 Berlin series: wall clock preserved
    /// across the 2026-03-29 DST change (08:00Z before, 07:00Z after).
    #[test]
    fn weekly_series_preserves_wall_clock_across_dst() {
        let slots = series_slots(
            &zoned(2026, 3, 2, 9, 0, Tz::Europe__Berlin),
            "FREQ=WEEKLY;INTERVAL=1;BYDAY=MO",
            utc_ms("2026-03-23T00:00:00Z"),
            utc_ms("2026-03-31T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(
            slots,
            vec![
                utc_ms("2026-03-23T08:00:00Z"),
                utc_ms("2026-03-30T07:00:00Z")
            ]
        );
    }

    /// Biweekly phase anchoring: 03-03 in, 03-10 out, 03-17 in.
    #[test]
    fn biweekly_keeps_its_phase() {
        let slots = series_slots(
            &zoned(2026, 3, 3, 10, 0, Tz::Europe__Berlin),
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU;UNTIL=20260414T080000Z",
            utc_ms("2026-03-01T00:00:00Z"),
            utc_ms("2026-03-20T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(
            slots,
            vec![
                utc_ms("2026-03-03T09:00:00Z"),
                utc_ms("2026-03-17T09:00:00Z")
            ]
        );
    }

    /// The UNTIL boundary occurrence (exactly equal instant) is included.
    #[test]
    fn until_boundary_occurrence_is_inclusive() {
        let slots = series_slots(
            &zoned(2026, 3, 3, 10, 0, Tz::Europe__Berlin),
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU;UNTIL=20260414T080000Z",
            utc_ms("2026-04-01T00:00:00Z"),
            utc_ms("2026-05-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(slots, vec![utc_ms("2026-04-14T08:00:00Z")]);
    }

    #[test]
    fn monthly_second_friday() {
        let slots = series_slots(
            &zoned(2026, 1, 9, 14, 0, Tz::Europe__Berlin),
            "FREQ=MONTHLY;INTERVAL=1;BYDAY=2FR",
            utc_ms("2026-03-01T00:00:00Z"),
            utc_ms("2026-04-30T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(
            slots,
            vec![
                utc_ms("2026-03-13T13:00:00Z"),
                utc_ms("2026-04-10T12:00:00Z")
            ]
        );
    }

    /// Z-less UNTIL resolves in the EVENT's timezone (node-ical parity,
    /// verified empirically): 08:00 wall Berlin on 2026-04-14 is 06:00Z, so
    /// the 08:00Z occurrence that day is past UNTIL and must be excluded —
    /// while the previous occurrence (2026-03-31, after the DST change)
    /// still lands.
    #[test]
    fn zless_until_is_resolved_in_the_events_timezone() {
        let april = series_slots(
            &zoned(2026, 3, 3, 10, 0, Tz::Europe__Berlin),
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU;UNTIL=20260414T080000",
            utc_ms("2026-04-01T00:00:00Z"),
            utc_ms("2026-05-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(april, Vec::<i64>::new());

        let march = series_slots(
            &zoned(2026, 3, 3, 10, 0, Tz::Europe__Berlin),
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU;UNTIL=20260414T080000",
            utc_ms("2026-03-20T00:00:00Z"),
            utc_ms("2026-04-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(march, vec![utc_ms("2026-03-31T08:00:00Z")]);
    }

    /// A date-only Z-less UNTIL means END of that day in the event's zone
    /// (node-ical parity: UNTIL=20260414 with a Berlin start resolves to
    /// 21:59:59Z), so the occurrence on the UNTIL date itself still lands.
    #[test]
    fn date_only_until_includes_the_occurrence_on_the_until_date() {
        let slots = series_slots(
            &zoned(2026, 3, 3, 10, 0, Tz::Europe__Berlin),
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU;UNTIL=20260414",
            utc_ms("2026-04-01T00:00:00Z"),
            utc_ms("2026-05-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(slots, vec![utc_ms("2026-04-14T08:00:00Z")]);
    }

    #[test]
    fn hostile_rules_hit_the_iteration_cap_instead_of_hanging() {
        let err = series_slots(
            &zoned(2020, 1, 1, 0, 0, Tz::Europe__Berlin),
            "FREQ=SECONDLY",
            utc_ms("2026-01-01T00:00:00Z"),
            utc_ms("2026-01-02T00:00:00Z"),
        )
        .unwrap_err();
        assert!(err.0.contains("iterations"));
    }
}
