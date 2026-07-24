//! Recurrence expansion — the correctness core, a line-by-line port of the
//! TS `expand.ts`. Handles RRULE series, EXDATE (timed and date-only,
//! multi-value, TZID-qualified), RECURRENCE-ID overrides (in place, moved
//! across days, or moved into the window from outside), orphan overrides
//! whose master is not in the feed, and all-day events with exclusive DTEND.
//!
//! Timed instances live in the absolute-instant domain (epoch ms); all-day
//! instances stay in the calendar-date domain (never converted through fake
//! midnights — end date is exclusive per RFC 5545). Nothing here consults
//! the host timezone.

use std::collections::{BTreeMap, HashSet};

use chrono::{Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::errors::AppError;
use crate::ics::model::{IcsDateTime, ParsedCalendar, ParsedEvent};
use crate::ics::rrule_slots::{SlotError, series_slots};

pub const MS_PER_DAY: i64 = 86_400_000;
const MAX_RANGE_DAYS: i64 = 31;

#[derive(Debug, Clone, PartialEq)]
pub struct EventWindow {
    /// Calendar-date lower bound, inclusive.
    pub start_date: NaiveDate,
    /// Calendar-date upper bound, EXCLUSIVE.
    pub end_date: NaiveDate,
    /// Absolute instant lower bound, inclusive (epoch ms).
    pub start_ms: i64,
    /// Absolute instant upper bound, exclusive (epoch ms).
    pub end_ms: i64,
    pub tz: Tz,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InstanceTime {
    Timed {
        start_ms: i64,
        end_ms: i64,
    },
    /// Calendar-date domain; `end` is exclusive.
    AllDay {
        start: NaiveDate,
        end: NaiveDate,
    },
}

/// One concrete occurrence, borrowing its source event (no clones).
#[derive(Debug, Clone, Copy)]
pub struct ExpandedInstance<'a> {
    pub event: &'a ParsedEvent,
    pub time: InstanceTime,
    pub is_recurring: bool,
}

impl ExpandedInstance<'_> {
    pub fn is_all_day(&self) -> bool {
        matches!(self.time, InstanceTime::AllDay { .. })
    }
}

/// Local wall time → zoned instant with Temporal's 'compatible'
/// disambiguation: folds pick the earlier offset, gaps shift forward by the
/// wall-clock duration from midnight (the same strategy rust-rrule uses
/// internally). Replaces `Temporal.PlainDate.toZonedDateTime(tz)` — DST-day
/// windows come out 23 h / 25 h with no ±86_400_000 arithmetic anywhere.
pub fn resolve_local(tz: Tz, naive: NaiveDateTime) -> chrono::DateTime<Tz> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(earlier, _later) => earlier,
        LocalResult::None => {
            let midnight = naive.date().and_time(NaiveTime::MIN);
            match tz.from_local_datetime(&midnight) {
                LocalResult::Single(base) | LocalResult::Ambiguous(base, _) => {
                    base + (naive - midnight)
                }
                LocalResult::None => {
                    // Midnight itself sits in a gap (zones transitioning at
                    // 00:00) — anchor on the previous day's midnight.
                    let prev = midnight - Duration::days(1);
                    let base = tz
                        .from_local_datetime(&prev)
                        .earliest()
                        .unwrap_or_else(|| tz.from_utc_datetime(&prev));
                    base + (naive - prev)
                }
            }
        }
    }
}

/// Absolute instant of a parsed datetime. All-day dates map to UTC midnight —
/// the same domain their RRULE slots are generated in.
pub fn instant_ms(dt: &IcsDateTime) -> i64 {
    match dt {
        IcsDateTime::Date(d) => Utc
            .from_utc_datetime(&d.and_time(NaiveTime::MIN))
            .timestamp_millis(),
        IcsDateTime::Utc(n) | IcsDateTime::Floating(n) => n.and_utc().timestamp_millis(),
        IcsDateTime::Zoned { local, tz } => resolve_local(*tz, *local).timestamp_millis(),
    }
}

/// Calendar-date part, without any zone conversion (mirrors the TS
/// `localPlainDate` intent, minus the system-zone round-trip).
fn date_part(dt: &IcsDateTime) -> NaiveDate {
    match dt {
        IcsDateTime::Date(d) => *d,
        IcsDateTime::Utc(n) | IcsDateTime::Floating(n) => n.date(),
        IcsDateTime::Zoned { local, .. } => local.date(),
    }
}

fn utc_ms_date(ms: i64) -> NaiveDate {
    chrono::DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.date_naive())
        .unwrap_or_default()
}

fn instant_date(ms: i64, tz: Tz) -> NaiveDate {
    chrono::DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.with_timezone(&tz).date_naive())
        .unwrap_or_default()
}

fn plain_date(iso: &str, field: &str) -> Result<NaiveDate, AppError> {
    NaiveDate::parse_from_str(iso, "%Y-%m-%d").map_err(|_| {
        AppError::InvalidRange(format!(
            "{field} must be a valid calendar date in YYYY-MM-DD format (got \"{iso}\")"
        ))
    })
}

fn window_from_dates(start: NaiveDate, end_exclusive: NaiveDate, tz: Tz) -> EventWindow {
    EventWindow {
        start_ms: resolve_local(tz, start.and_time(NaiveTime::MIN)).timestamp_millis(),
        end_ms: resolve_local(tz, end_exclusive.and_time(NaiveTime::MIN)).timestamp_millis(),
        start_date: start,
        end_date: end_exclusive,
        tz,
    }
}

/// Window for one local day; `date_iso: None` means today in `tz`, derived
/// from `now_ms` (the injected feed clock, so tests control it).
pub fn day_window(date_iso: Option<&str>, tz: Tz, now_ms: i64) -> Result<EventWindow, AppError> {
    let date = match date_iso {
        None => chrono::DateTime::<Utc>::from_timestamp_millis(now_ms)
            .map(|dt| dt.with_timezone(&tz).date_naive())
            .unwrap_or_default(),
        Some(iso) => plain_date(iso, "date")?,
    };
    Ok(window_from_dates(date, date + Duration::days(1), tz))
}

/// Window for an inclusive date range; max 31 days.
pub fn range_window(from_iso: &str, to_iso: &str, tz: Tz) -> Result<EventWindow, AppError> {
    let from = plain_date(from_iso, "from")?;
    let to = plain_date(to_iso, "to")?;
    if to < from {
        return Err(AppError::InvalidRange(format!(
            "\"from\" ({from_iso}) must be on or before \"to\" ({to_iso})"
        )));
    }
    let inclusive_days = (to - from).num_days() + 1;
    if inclusive_days > MAX_RANGE_DAYS {
        return Err(AppError::InvalidRange(format!(
            "range spans {inclusive_days} days; the maximum is {MAX_RANGE_DAYS} days"
        )));
    }
    Ok(window_from_dates(from, to + Duration::days(1), tz))
}

fn timed_overlaps(win: &EventWindow, start_ms: i64, end_ms: i64) -> bool {
    if start_ms == end_ms {
        // Zero-length events use half-open containment.
        start_ms >= win.start_ms && start_ms < win.end_ms
    } else {
        start_ms < win.end_ms && end_ms > win.start_ms
    }
}

fn overlaps_window(win: &EventWindow, inst: &ExpandedInstance<'_>) -> bool {
    match inst.time {
        InstanceTime::Timed { start_ms, end_ms } => timed_overlaps(win, start_ms, end_ms),
        InstanceTime::AllDay { start, end } => start < win.end_date && end > win.start_date,
    }
}

/// Instance built from a component's own DTSTART/DTEND (non-recurring
/// events, overrides, orphans).
fn from_component(component: &ParsedEvent, is_recurring: bool) -> ExpandedInstance<'_> {
    if component.is_all_day() {
        let start = date_part(&component.dtstart);
        let mut end = component
            .dtend
            .as_ref()
            .map(date_part)
            .unwrap_or_else(|| start + Duration::days(1));
        if end <= start {
            end = start + Duration::days(1);
        }
        return ExpandedInstance {
            event: component,
            time: InstanceTime::AllDay { start, end },
            is_recurring,
        };
    }
    let start_ms = instant_ms(&component.dtstart);
    // Malformed feeds can carry DTEND before DTSTART — clamp to zero-length
    // instead of producing a negative interval that never overlaps anything.
    let end_ms = component
        .dtend
        .as_ref()
        .map(instant_ms)
        .unwrap_or(start_ms)
        .max(start_ms);
    ExpandedInstance {
        event: component,
        time: InstanceTime::Timed { start_ms, end_ms },
        is_recurring,
    }
}

/// Expand every event group into concrete instances overlapping the window.
/// Dedup key: uid + ORIGINAL slot instant (RECURRENCE-ID) — never the moved
/// start; first wins. This is the invariant that prevents a master
/// occurrence and its override from both being returned.
pub fn expand_events<'a>(
    cal: &'a ParsedCalendar,
    win: &EventWindow,
) -> Result<Vec<ExpandedInstance<'a>>, SlotError> {
    let mut seen: HashSet<(&'a str, i64)> = HashSet::new();
    let mut out: Vec<ExpandedInstance<'a>> = Vec::new();
    let emit = |seen: &mut HashSet<(&'a str, i64)>,
                out: &mut Vec<ExpandedInstance<'a>>,
                uid: &'a str,
                slot_ms: i64,
                inst: ExpandedInstance<'a>| {
        if seen.insert((uid, slot_ms)) {
            out.push(inst);
        }
    };

    for group in &cal.groups {
        let Some(master) = &group.master else {
            // Orphan overrides whose master is absent from the feed
            // (Exchange emits these) — each is a single instance keyed by
            // its original slot.
            for ov in &group.overrides {
                let Some(rid) = &ov.recurrence_id else {
                    continue;
                };
                let inst = from_component(ov, true);
                if overlaps_window(win, &inst) {
                    emit(&mut seen, &mut out, &ov.uid, instant_ms(rid), inst);
                }
            }
            continue;
        };

        let Some(rrule_raw) = &master.rrule_raw else {
            // Non-recurring event (attached overrides, if any, are ignored —
            // TS behavior).
            let inst = from_component(master, false);
            if overlaps_window(win, &inst) {
                emit(
                    &mut seen,
                    &mut out,
                    &master.uid,
                    instant_ms(&master.dtstart),
                    inst,
                );
            }
            continue;
        };

        // --- RRULE series ---
        let all_day = master.is_all_day();
        let start_ms = instant_ms(&master.dtstart);
        let dur_ms = master
            .dtend
            .as_ref()
            .map(instant_ms)
            .map(|end| (end - start_ms).max(0))
            .unwrap_or(0);
        let dur_days = if all_day {
            master
                .dtend
                .as_ref()
                .map(|end| (date_part(end) - date_part(&master.dtstart)).num_days())
                .unwrap_or(1)
                .max(1)
        } else {
            0
        };

        // EXDATE match sets, built once per master. Timed EXDATEs match by
        // epoch ms; date-only EXDATEs match the occurrence's calendar day.
        let mut ex_times: HashSet<i64> = HashSet::new();
        let mut ex_dates: HashSet<NaiveDate> = HashSet::new();
        for ex in &master.exdates {
            match ex {
                IcsDateTime::Date(d) => {
                    ex_dates.insert(*d);
                }
                other => {
                    ex_times.insert(instant_ms(other));
                }
            }
        }

        let event_tz = match &master.dtstart {
            IcsDateTime::Zoned { tz, .. } => *tz,
            IcsDateTime::Utc(_) => Tz::UTC,
            _ => win.tz,
        };

        // Overrides keyed by the ORIGINAL slot instant (RECURRENCE-ID).
        // BTreeMap for deterministic emission order of moved-in overrides.
        let overrides: BTreeMap<i64, &ParsedEvent> = group
            .overrides
            .iter()
            .filter_map(|ov| ov.recurrence_id.as_ref().map(|rid| (instant_ms(rid), ov)))
            .collect();

        // Widen the query backwards so occurrences that start before the
        // window but overlap into it (e.g. 23:30–00:30) are found.
        let widen_ms = if all_day {
            (dur_days + 1) * MS_PER_DAY
        } else {
            dur_ms
        };
        let slots = series_slots(
            &master.dtstart,
            rrule_raw,
            win.start_ms - widen_ms,
            win.end_ms,
        )?;

        let mut consumed: HashSet<i64> = HashSet::new();
        for slot_ms in slots {
            if ex_times.contains(&slot_ms) {
                continue;
            }
            if !ex_dates.is_empty() {
                let slot_date = if all_day {
                    utc_ms_date(slot_ms)
                } else {
                    instant_date(slot_ms, event_tz)
                };
                if ex_dates.contains(&slot_date) {
                    continue;
                }
            }

            if let Some(ov) = overrides.get(&slot_ms) {
                // Override replaces the base slot — even when its new time
                // moved out of the window, the base occurrence must not
                // reappear.
                consumed.insert(slot_ms);
                let inst = from_component(ov, true);
                if overlaps_window(win, &inst) {
                    emit(&mut seen, &mut out, &master.uid, slot_ms, inst);
                }
                continue;
            }

            let inst = if all_day {
                let start = utc_ms_date(slot_ms);
                ExpandedInstance {
                    event: master,
                    time: InstanceTime::AllDay {
                        start,
                        end: start + Duration::days(dur_days),
                    },
                    is_recurring: true,
                }
            } else {
                ExpandedInstance {
                    event: master,
                    time: InstanceTime::Timed {
                        start_ms: slot_ms,
                        end_ms: slot_ms + dur_ms,
                    },
                    is_recurring: true,
                }
            };
            if overlaps_window(win, &inst) {
                emit(&mut seen, &mut out, &master.uid, slot_ms, inst);
            }
        }

        // Overrides whose original slot lies outside the queried slots
        // (moved into the window from another day, or sitting on an
        // EXDATE'd slot — override wins over EXDATE by design).
        for (slot_ms, ov) in overrides {
            if consumed.contains(&slot_ms) {
                continue;
            }
            let inst = from_component(ov, true);
            if overlaps_window(win, &inst) {
                emit(&mut seen, &mut out, &master.uid, slot_ms, inst);
            }
        }
    }

    // Sort: all-day events at their local midnight in the window's zone (so
    // they precede same-morning timed events), then start instant, then
    // summary as the tiebreak (codepoint order — documented deviation from
    // localeCompare).
    let sort_ms = |inst: &ExpandedInstance<'_>| match inst.time {
        InstanceTime::Timed { start_ms, .. } => start_ms,
        InstanceTime::AllDay { start, .. } => {
            resolve_local(win.tz, start.and_time(NaiveTime::MIN)).timestamp_millis()
        }
    };
    out.sort_by(|a, b| {
        sort_ms(a).cmp(&sort_ms(b)).then_with(|| {
            let sa = a.event.summary.as_deref().unwrap_or("");
            let sb = b.event.summary.as_deref().unwrap_or("");
            sa.cmp(sb)
        })
    });
    Ok(out)
}
