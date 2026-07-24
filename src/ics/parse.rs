//! Hand-rolled ICS content-line parser (replaces node-ical). Input is already
//! unfolded and TZID-normalized by `tzids.rs`. Deliberately minimal: only the
//! grammar this Exchange-feed server needs — BEGIN/END component tracking,
//! `NAME(;PARAM=VAL|"VAL")*(:VALUE)` lines, VALUE=DATE / TZID / Z datetime
//! literals, EXDATE multi-value, RFC 5545 §3.3.11 text unescaping.

use std::collections::HashMap;

use chrono::{NaiveDate, NaiveDateTime};
use chrono_tz::Tz;

use crate::ics::expand::instant_ms;
use crate::ics::model::{EventGroup, IcsDateTime, ParsedCalendar, ParsedEvent};
use crate::ics::tzids::{is_valid_iana, split_quote_aware, value_separator_index};

/// Parse failure — truncated or malformed feeds. The description is built
/// from structural information only (never property values), so it is safe
/// for stderr.
#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// RFC 5545 §3.3.11 TEXT unescaping: `\n`/`\N` → newline, any other escaped
/// character → itself (`\\`, `\,`, `\;` — lenient like node-ical).
fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn param<'a>(params: &[&'a str], key: &str) -> Option<&'a str> {
    params.iter().find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.eq_ignore_ascii_case(key).then(|| v.trim_matches('"'))
    })
}

fn parse_date(value: &str) -> Result<NaiveDate, ParseError> {
    NaiveDate::parse_from_str(value, "%Y%m%d")
        .map_err(|_| ParseError("invalid DATE literal".to_string()))
}

/// One datetime literal with its line's parameters applied.
fn parse_datetime(value: &str, params: &[&str], tz_default: Tz) -> Result<IcsDateTime, ParseError> {
    let value_is_date = param(params, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
    if value_is_date || (value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit())) {
        return Ok(IcsDateTime::Date(parse_date(value)?));
    }
    let (body, is_utc) = match value.strip_suffix('Z') {
        Some(body) => (body, true),
        None => (value, false),
    };
    let local = NaiveDateTime::parse_from_str(body, "%Y%m%dT%H%M%S")
        .map_err(|_| ParseError("invalid DATE-TIME literal".to_string()))?;
    if is_utc {
        return Ok(IcsDateTime::Utc(local));
    }
    match param(params, "TZID") {
        // Post-normalization every TZID is valid IANA; fall back defensively.
        Some(tzid) => Ok(IcsDateTime::Zoned {
            local,
            tz: is_valid_iana(tzid).unwrap_or(tz_default),
        }),
        None => Ok(IcsDateTime::Floating(local)),
    }
}

/// Best-effort RFC 5545 §3.3.6 DURATION: `[+-]P(nW | nD)(T nH nM nS)`.
/// `None` for anything malformed or overflowing — every arithmetic step is
/// checked because a hostile value must never panic (the release profile
/// aborts on panic, so an unchecked overflow would kill the server on the
/// first fetch of a bad feed).
fn parse_duration(value: &str) -> Option<chrono::Duration> {
    let (sign, rest) = match value.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;
    let (date_part, time_part) = match rest.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (rest, None),
    };
    let mut seconds: i64 = 0;
    let mut accumulate = |part: &str, units: &[(char, i64)]| -> Option<()> {
        let mut digits = String::new();
        for ch in part.chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
            } else {
                let n: i64 = digits.parse().ok()?;
                digits.clear();
                let (_, unit_seconds) = units.iter().find(|(designator, _)| *designator == ch)?;
                seconds = seconds.checked_add(n.checked_mul(*unit_seconds)?)?;
            }
        }
        digits.is_empty().then_some(())
    };
    accumulate(date_part, &[('W', 604_800), ('D', 86_400)])?;
    if let Some(time_part) = time_part {
        accumulate(time_part, &[('H', 3600), ('M', 60), ('S', 1)])?;
    }
    chrono::Duration::try_seconds(sign.checked_mul(seconds)?)
}

/// node-ical parity: with no DTEND, `end = start + DURATION` — absolute
/// arithmetic for timed starts, whole days for all-day dates. `None` when
/// the result is unrepresentable (the event then has no synthesized end).
fn end_from_duration(dtstart: &IcsDateTime, duration: chrono::Duration) -> Option<IcsDateTime> {
    match dtstart {
        IcsDateTime::Date(d) => d.checked_add_signed(duration).map(IcsDateTime::Date),
        timed => instant_ms(timed)
            .checked_add(duration.num_milliseconds())
            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
            .map(|dt| IcsDateTime::Utc(dt.naive_utc())),
    }
}

#[derive(Default)]
struct EventBuilder {
    uid: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    busy_status_raw: Option<String>,
    teams_url_prop: Option<String>,
    dtstart: Option<IcsDateTime>,
    dtend: Option<IcsDateTime>,
    duration: Option<chrono::Duration>,
    rrule_raw: Option<String>,
    exdates: Vec<IcsDateTime>,
    recurrence_id: Option<IcsDateTime>,
    sequence: i64,
}

impl EventBuilder {
    fn finish(self) -> Result<ParsedEvent, ParseError> {
        let uid = self
            .uid
            .ok_or_else(|| ParseError("VEVENT without UID".to_string()))?;
        let dtstart = self
            .dtstart
            .ok_or_else(|| ParseError("VEVENT without DTSTART".to_string()))?;
        // DTEND wins over DURATION when a feed (invalidly) carries both —
        // node-ical parity, verified empirically.
        let dtend = match (self.dtend, self.duration) {
            (Some(end), _) => Some(end),
            (None, Some(duration)) => end_from_duration(&dtstart, duration),
            (None, None) => None,
        };
        Ok(ParsedEvent {
            uid,
            summary: self.summary,
            description: self.description,
            location: self.location,
            busy_status_raw: self.busy_status_raw,
            teams_url_prop: self.teams_url_prop,
            dtstart,
            dtend,
            rrule_raw: self.rrule_raw,
            exdates: self.exdates,
            recurrence_id: self.recurrence_id,
            sequence: self.sequence,
        })
    }
}

/// node-ical parity (verified against its source and empirically): a
/// winning duplicate component MERGES key by key onto the stored one —
/// properties absent from the newer component survive from the older, so a
/// mid-update snapshot that re-emits only the changed fields cannot erase
/// the title or resurrect an EXDATE'd occurrence.
fn merge_component(existing: &mut ParsedEvent, newer: ParsedEvent) {
    let ParsedEvent {
        uid: _,
        summary,
        description,
        location,
        busy_status_raw,
        teams_url_prop,
        dtstart,
        dtend,
        rrule_raw,
        exdates,
        recurrence_id,
        sequence,
    } = newer;
    existing.dtstart = dtstart;
    existing.sequence = sequence;
    if summary.is_some() {
        existing.summary = summary;
    }
    if description.is_some() {
        existing.description = description;
    }
    if location.is_some() {
        existing.location = location;
    }
    if busy_status_raw.is_some() {
        existing.busy_status_raw = busy_status_raw;
    }
    if teams_url_prop.is_some() {
        existing.teams_url_prop = teams_url_prop;
    }
    if dtend.is_some() {
        existing.dtend = dtend;
    }
    if rrule_raw.is_some() {
        existing.rrule_raw = rrule_raw;
    }
    if !exdates.is_empty() {
        existing.exdates = exdates;
    }
    if recurrence_id.is_some() {
        existing.recurrence_id = recurrence_id;
    }
}

/// Parse a normalized, unfolded feed into UID-grouped events. Only VEVENTs
/// that are direct children of VCALENDAR materialize; VTIMEZONE (whose
/// STANDARD/DAYLIGHT blocks carry their own DTSTART/RRULE lines) and VALARM
/// are skipped structurally. Errors on truncated input (unterminated
/// component) — a null diagnosis for feed_info, an internal error for tools.
pub fn parse_calendar(ics: &str, tz_default: Tz) -> Result<ParsedCalendar, ParseError> {
    let mut stack: Vec<String> = Vec::new();
    let mut current: Option<EventBuilder> = None;
    let mut events: Vec<ParsedEvent> = Vec::new();

    for line in ics.lines() {
        if line.is_empty() {
            continue;
        }
        // Lines without a value separator are structurally meaningless —
        // skipped leniently (node-ical does the same).
        let Some(sep) = value_separator_index(line) else {
            continue;
        };
        let head = &line[..sep];
        let value = &line[sep + 1..];
        let params = split_quote_aware(head, ';');
        let name = params.first().copied().unwrap_or("").to_ascii_uppercase();

        if name == "BEGIN" {
            let component = value.trim().to_ascii_uppercase();
            if component == "VEVENT" && stack.len() == 1 && current.is_none() {
                current = Some(EventBuilder::default());
            }
            stack.push(component);
            continue;
        }
        if name == "END" {
            let component = value.trim().to_ascii_uppercase();
            match stack.pop() {
                Some(open) if open == component => {}
                _ => return Err(ParseError(format!("unbalanced END:{component}"))),
            }
            if component == "VEVENT" && stack.len() == 1 {
                if let Some(builder) = current.take() {
                    events.push(builder.finish()?);
                }
            }
            continue;
        }

        // Collect only properties that sit directly inside a VEVENT (depth 2:
        // VCALENDAR > VEVENT) — never VALARM internals or VTIMEZONE blocks.
        let in_vevent = stack.len() == 2 && stack.last().is_some_and(|c| c == "VEVENT");
        let Some(builder) = current.as_mut().filter(|_| in_vevent) else {
            continue;
        };
        match name.as_str() {
            "UID" => builder.uid = Some(unescape_text(value)),
            "SUMMARY" => builder.summary = Some(unescape_text(value)),
            "DESCRIPTION" => builder.description = Some(unescape_text(value)),
            "LOCATION" => builder.location = Some(unescape_text(value)),
            "X-MICROSOFT-CDO-BUSYSTATUS" => builder.busy_status_raw = Some(unescape_text(value)),
            "X-MICROSOFT-SKYPETEAMSMEETINGURL" => {
                builder.teams_url_prop = Some(unescape_text(value));
            }
            "DTSTART" => builder.dtstart = Some(parse_datetime(value, &params, tz_default)?),
            "DTEND" => builder.dtend = Some(parse_datetime(value, &params, tz_default)?),
            // Lenient like node-ical: a malformed or overflowing DURATION
            // degrades to zero length (warned) — it must never fail or crash
            // the whole feed.
            "DURATION" => {
                builder.duration = Some(parse_duration(value).unwrap_or_else(|| {
                    crate::logger::warn(
                        "Ignoring unusable DURATION in feed; treating it as zero length",
                        &[("duration", serde_json::json!(value))],
                    );
                    chrono::Duration::zero()
                }));
            }
            // Lenient like node-ical: an unparseable SEQUENCE counts as 0.
            "SEQUENCE" => builder.sequence = value.trim().parse().unwrap_or(0),
            "RRULE" => builder.rrule_raw = Some(value.to_string()),
            "EXDATE" => {
                for part in value.split(',') {
                    if !part.is_empty() {
                        builder
                            .exdates
                            .push(parse_datetime(part, &params, tz_default)?);
                    }
                }
            }
            "RECURRENCE-ID" => {
                builder.recurrence_id = Some(parse_datetime(value, &params, tz_default)?);
            }
            _ => {}
        }
    }

    if !stack.is_empty() {
        return Err(ParseError(format!(
            "unterminated component: {}",
            stack.join(" > ")
        )));
    }

    // Group by UID in feed order: master = the VEVENT without RECURRENCE-ID,
    // overrides attach to it regardless of their position in the feed.
    let mut groups: Vec<EventGroup> = Vec::new();
    let mut index_by_uid: HashMap<String, usize> = HashMap::new();
    for ev in events {
        match index_by_uid.get(&ev.uid) {
            Some(&i) => {
                if ev.recurrence_id.is_some() {
                    // Duplicate override for the same original slot: the
                    // higher SEQUENCE wins, ties go to the later component
                    // (node-ical parity, verified empirically). Identity is
                    // the exact original slot instant — deliberately NOT
                    // node-ical's calendar-day key, which drops a legitimate
                    // second override for a different slot on the same day.
                    let rid_ms = ev.recurrence_id.as_ref().map(instant_ms);
                    let existing = groups[i]
                        .overrides
                        .iter_mut()
                        .find(|o| o.recurrence_id.as_ref().map(instant_ms) == rid_ms);
                    match existing {
                        Some(existing) if ev.sequence < existing.sequence => {}
                        Some(existing) => merge_component(existing, ev),
                        None => groups[i].overrides.push(ev),
                    }
                } else {
                    // Duplicate master for a UID: higher SEQUENCE wins, ties
                    // go to the later component. Exchange does not emit
                    // duplicates, but a mid-update feed snapshot can.
                    match &mut groups[i].master {
                        Some(existing) if ev.sequence < existing.sequence => {}
                        Some(existing) => merge_component(existing, ev),
                        None => groups[i].master = Some(ev),
                    }
                }
            }
            None => {
                index_by_uid.insert(ev.uid.clone(), groups.len());
                let group = if ev.recurrence_id.is_some() {
                    EventGroup {
                        master: None,
                        overrides: vec![ev],
                    }
                } else {
                    EventGroup {
                        master: Some(ev),
                        overrides: Vec::new(),
                    }
                };
                groups.push(group);
            }
        }
    }
    Ok(ParsedCalendar { groups })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime};

    use super::*;
    use crate::ics::tzids::{normalize_tzids, unfold_ics};

    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/feed.ics"
    ));
    const FIXTURE_TRUNCATED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/feed-truncated.ics"
    ));
    const TZ: Tz = Tz::Europe__Prague;

    fn parse_fixture() -> ParsedCalendar {
        let normalized = normalize_tzids(&unfold_ics(FIXTURE), TZ);
        parse_calendar(&normalized.ics, TZ).unwrap()
    }

    fn group<'a>(cal: &'a ParsedCalendar, uid: &str) -> &'a EventGroup {
        cal.groups
            .iter()
            .find(|g| {
                g.master
                    .as_ref()
                    .or_else(|| g.overrides.first())
                    .is_some_and(|e| e.uid == uid)
            })
            .unwrap()
    }

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn parses_the_fixture_into_sixteen_groups_with_two_overrides() {
        let cal = parse_fixture();
        assert_eq!(cal.groups.len(), 16);
        let total: usize = cal
            .groups
            .iter()
            .map(|g| usize::from(g.master.is_some()) + g.overrides.len())
            .sum();
        assert_eq!(total, 18);
        assert_eq!(group(&cal, "weekly-ovr@fixture").overrides.len(), 1);
        assert_eq!(group(&cal, "weekly-ovr-moved@fixture").overrides.len(), 1);
    }

    #[test]
    fn resolves_windows_tzids_to_zoned_wall_times() {
        let cal = parse_fixture();
        let ev = group(&cal, "weekly-mon@fixture").master.as_ref().unwrap();
        assert_eq!(
            ev.dtstart,
            IcsDateTime::Zoned {
                local: local(2026, 3, 2, 9, 0),
                tz: Tz::Europe__Berlin,
            }
        );
        assert_eq!(
            ev.rrule_raw.as_deref(),
            Some("FREQ=WEEKLY;INTERVAL=1;BYDAY=MO")
        );

        let cest = group(&cal, "cest-tzid@fixture").master.as_ref().unwrap();
        assert!(matches!(
            cest.dtstart,
            IcsDateTime::Zoned {
                tz: Tz::Europe__Budapest,
                ..
            }
        ));

        // Unknown TZID was rewritten to the default zone, never the host zone.
        let unknown = group(&cal, "unknown-tzid@fixture").master.as_ref().unwrap();
        assert!(matches!(
            unknown.dtstart,
            IcsDateTime::Zoned {
                tz: Tz::Europe__Prague,
                ..
            }
        ));
    }

    #[test]
    fn merges_folded_multi_value_exdates() {
        let cal = parse_fixture();
        let ev = group(&cal, "weekly-exdate@fixture")
            .master
            .as_ref()
            .unwrap();
        assert_eq!(
            ev.exdates,
            vec![
                IcsDateTime::Zoned {
                    local: local(2026, 3, 9, 11, 0),
                    tz: Tz::Europe__Berlin
                },
                IcsDateTime::Zoned {
                    local: local(2026, 3, 23, 11, 0),
                    tz: Tz::Europe__Berlin
                },
                IcsDateTime::Zoned {
                    local: local(2026, 4, 6, 11, 0),
                    tz: Tz::Europe__Berlin
                },
            ]
        );
    }

    #[test]
    fn keeps_all_day_events_in_the_calendar_date_domain() {
        let cal = parse_fixture();
        let ev = group(&cal, "allday-multi@fixture").master.as_ref().unwrap();
        assert!(ev.is_all_day());
        assert_eq!(
            ev.dtstart,
            IcsDateTime::Date(NaiveDate::from_ymd_opt(2026, 3, 16).unwrap())
        );
        assert_eq!(
            ev.dtend,
            Some(IcsDateTime::Date(
                NaiveDate::from_ymd_opt(2026, 3, 19).unwrap()
            ))
        );
    }

    #[test]
    fn unescapes_text_and_reassembles_folded_lines() {
        let cal = parse_fixture();
        let ev = group(&cal, "escape@fixture").master.as_ref().unwrap();
        assert_eq!(ev.summary.as_deref(), Some("Q3 review, part 1; plán"));
        let description = ev.description.as_deref().unwrap();
        assert!(description.starts_with("line one\nline two, with a comma"));
        assert!(description.contains("diakritika ěščřžýáíé"));
        assert!(!description.contains('\\'));
    }

    #[test]
    fn captures_the_teams_meeting_url_x_prop() {
        let cal = parse_fixture();
        let ev = group(&cal, "teams-xprop@fixture").master.as_ref().unwrap();
        assert!(
            ev.teams_url_prop
                .as_deref()
                .unwrap()
                .starts_with("https://teams.microsoft.com/l/meetup-join/")
        );
        assert_eq!(ev.busy_status_raw.as_deref(), Some("BUSY"));
    }

    #[test]
    fn override_events_carry_their_recurrence_id_slot() {
        let cal = parse_fixture();
        let ov = &group(&cal, "weekly-ovr@fixture").overrides[0];
        assert_eq!(
            ov.recurrence_id,
            Some(IcsDateTime::Zoned {
                local: local(2026, 3, 18, 13, 0),
                tz: Tz::Europe__Berlin,
            })
        );
        assert_eq!(ov.summary.as_deref(), Some("Team sync (moved)"));
    }

    #[test]
    fn errors_on_a_truncated_feed() {
        let normalized = normalize_tzids(&unfold_ics(FIXTURE_TRUNCATED), TZ);
        assert!(parse_calendar(&normalized.ics, TZ).is_err());
    }

    // --- DURATION and SEQUENCE: node-ical 0.26.1 parity, verified against
    // --- the real library (see the 2026-07 code-review fixes).

    fn parse_snippet(body: &str) -> ParsedCalendar {
        let ics = format!("BEGIN:VCALENDAR\r\n{}\r\nEND:VCALENDAR\r\n", body.trim());
        parse_calendar(&ics, TZ).unwrap()
    }

    fn only_master(cal: &ParsedCalendar) -> &crate::ics::model::ParsedEvent {
        cal.groups[0].master.as_ref().unwrap()
    }

    #[test]
    fn duration_synthesizes_the_end_when_dtend_is_absent() {
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:d1\r\nDTSTART;TZID=Europe/Berlin:20260303T100000\r\nDURATION:PT90M\r\nEND:VEVENT",
        );
        // 10:00 Berlin (CET) = 09:00Z; +90 min absolute = 10:30Z.
        assert_eq!(
            only_master(&cal).dtend,
            Some(IcsDateTime::Utc(local(2026, 3, 3, 10, 30)))
        );
    }

    #[test]
    fn duration_on_all_day_events_stays_in_the_date_domain() {
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:d2\r\nDTSTART;VALUE=DATE:20260316\r\nDURATION:P3D\r\nEND:VEVENT",
        );
        assert_eq!(
            only_master(&cal).dtend,
            Some(IcsDateTime::Date(
                NaiveDate::from_ymd_opt(2026, 3, 19).unwrap()
            ))
        );
        let weeks = parse_snippet(
            "BEGIN:VEVENT\r\nUID:d3\r\nDTSTART;VALUE=DATE:20260316\r\nDURATION:P2W\r\nEND:VEVENT",
        );
        assert_eq!(
            only_master(&weeks).dtend,
            Some(IcsDateTime::Date(
                NaiveDate::from_ymd_opt(2026, 3, 30).unwrap()
            ))
        );
    }

    #[test]
    fn dtend_wins_when_a_feed_carries_both_dtend_and_duration() {
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:d4\r\nDTSTART;TZID=Europe/Berlin:20260303T100000\r\nDTEND;TZID=Europe/Berlin:20260303T110000\r\nDURATION:PT15M\r\nEND:VEVENT",
        );
        assert_eq!(
            only_master(&cal).dtend,
            Some(IcsDateTime::Zoned {
                local: local(2026, 3, 3, 11, 0),
                tz: Tz::Europe__Berlin
            })
        );
    }

    #[test]
    fn duplicate_masters_keep_the_higher_sequence_regardless_of_order() {
        let newer_first = parse_snippet(
            "BEGIN:VEVENT\r\nUID:s\r\nSEQUENCE:2\r\nDTSTART;TZID=Europe/Berlin:20260303T100000\r\nSUMMARY:newer\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:s\r\nSEQUENCE:1\r\nDTSTART;TZID=Europe/Berlin:20260303T120000\r\nSUMMARY:older\r\nEND:VEVENT",
        );
        assert_eq!(only_master(&newer_first).summary.as_deref(), Some("newer"));

        let newer_second = parse_snippet(
            "BEGIN:VEVENT\r\nUID:s\r\nSEQUENCE:1\r\nDTSTART;TZID=Europe/Berlin:20260303T120000\r\nSUMMARY:older\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:s\r\nSEQUENCE:2\r\nDTSTART;TZID=Europe/Berlin:20260303T100000\r\nSUMMARY:newer\r\nEND:VEVENT",
        );
        assert_eq!(only_master(&newer_second).summary.as_deref(), Some("newer"));

        // Ties (and the no-SEQUENCE default of 0) go to the later component.
        let tie = parse_snippet(
            "BEGIN:VEVENT\r\nUID:s\r\nDTSTART;TZID=Europe/Berlin:20260303T100000\r\nSUMMARY:first\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:s\r\nDTSTART;TZID=Europe/Berlin:20260303T120000\r\nSUMMARY:second\r\nEND:VEVENT",
        );
        assert_eq!(only_master(&tie).summary.as_deref(), Some("second"));
    }

    #[test]
    fn hostile_durations_never_panic_and_degrade_to_zero_or_no_end() {
        // chrono::Duration::seconds would panic on this; try_seconds must not.
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:h1\r\nDTSTART;TZID=Europe/Berlin:20260303T100000\r\nDURATION:PT9223372036854775807S\r\nEND:VEVENT",
        );
        // Unusable value degrades to zero length: end == start instant.
        assert_eq!(
            only_master(&cal)
                .dtend
                .as_ref()
                .map(crate::ics::expand::instant_ms),
            Some(crate::ics::expand::instant_ms(&only_master(&cal).dtstart))
        );

        // NaiveDate + this duration would panic; checked_add_signed must not.
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:h2\r\nDTSTART;VALUE=DATE:20260316\r\nDURATION:P99999999D\r\nEND:VEVENT",
        );
        assert_eq!(only_master(&cal).dtend, None);

        // i64 multiply overflow in the accumulator.
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:h3\r\nDTSTART;TZID=Europe/Berlin:20260303T100000\r\nDURATION:P99999999999999999W\r\nEND:VEVENT",
        );
        assert!(cal.groups[0].master.is_some());
    }

    #[test]
    fn malformed_durations_never_fail_the_whole_feed() {
        // One bad DURATION must not take down the calendar (node-ical warns
        // and continues); the second event must still parse.
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:bad\r\nDTSTART;TZID=Europe/Berlin:20260303T100000\r\nDURATION:banana\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:good\r\nDTSTART;TZID=Europe/Berlin:20260304T100000\r\nDTEND;TZID=Europe/Berlin:20260304T110000\r\nSUMMARY:ok\r\nEND:VEVENT",
        );
        assert_eq!(cal.groups.len(), 2);
        let bad = cal.groups[0].master.as_ref().unwrap();
        assert_eq!(
            bad.dtend.as_ref().map(crate::ics::expand::instant_ms),
            Some(crate::ics::expand::instant_ms(&bad.dtstart))
        );
    }

    #[test]
    fn duplicate_masters_merge_so_fields_absent_from_the_newer_one_survive() {
        // node-ical merges key by key: a higher-SEQUENCE re-emit carrying
        // only the changed fields must not erase the title or resurrect the
        // EXDATE'd occurrence.
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:m\r\nSEQUENCE:1\r\nDTSTART;TZID=Europe/Berlin:20260304T130000\r\nDTEND;TZID=Europe/Berlin:20260304T133000\r\nRRULE:FREQ=WEEKLY;INTERVAL=1;BYDAY=WE\r\nEXDATE;TZID=Europe/Berlin:20260311T130000\r\nSUMMARY:old summary\r\nLOCATION:old location\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:m\r\nSEQUENCE:2\r\nDTSTART;TZID=Europe/Berlin:20260304T140000\r\nDTEND;TZID=Europe/Berlin:20260304T143000\r\nRRULE:FREQ=WEEKLY;INTERVAL=1;BYDAY=WE\r\nEND:VEVENT",
        );
        let master = only_master(&cal);
        assert_eq!(master.sequence, 2);
        assert_eq!(master.summary.as_deref(), Some("old summary"));
        assert_eq!(master.location.as_deref(), Some("old location"));
        assert_eq!(master.exdates.len(), 1);
        // The newer component's times win.
        assert_eq!(
            master.dtstart,
            IcsDateTime::Zoned {
                local: local(2026, 3, 4, 14, 0),
                tz: Tz::Europe__Berlin
            }
        );
    }

    #[test]
    fn duplicate_overrides_for_one_slot_keep_the_higher_sequence() {
        let cal = parse_snippet(
            "BEGIN:VEVENT\r\nUID:o\r\nDTSTART;TZID=Europe/Berlin:20260304T130000\r\nDTEND;TZID=Europe/Berlin:20260304T133000\r\nRRULE:FREQ=WEEKLY;INTERVAL=1;BYDAY=WE\r\nSUMMARY:master\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:o\r\nSEQUENCE:5\r\nRECURRENCE-ID;TZID=Europe/Berlin:20260318T130000\r\nDTSTART;TZID=Europe/Berlin:20260318T150000\r\nSUMMARY:keep\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:o\r\nSEQUENCE:4\r\nRECURRENCE-ID;TZID=Europe/Berlin:20260318T130000\r\nDTSTART;TZID=Europe/Berlin:20260318T160000\r\nSUMMARY:drop\r\nEND:VEVENT",
        );
        let overrides = &cal.groups[0].overrides;
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].summary.as_deref(), Some("keep"));
    }
}
