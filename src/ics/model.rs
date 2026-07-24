//! Semantic event model produced by the parser. Datetimes stay in the domain
//! the feed declared them in (calendar date vs. wall time + zone vs. UTC
//! instant); conversion to absolute instants happens at expansion time.

use chrono::{NaiveDate, NaiveDateTime};
use chrono_tz::Tz;

/// A datetime as written in the feed, post-TZID-normalization (every TZID is
/// valid IANA by the time parsing runs; the parser still falls back to the
/// default zone defensively).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IcsDateTime {
    /// `VALUE=DATE` — the all-day domain. Never touches midnights or zones.
    Date(NaiveDate),
    /// `…Z` — an absolute UTC instant.
    Utc(NaiveDateTime),
    /// `TZID=<IANA>` — wall time in a named zone.
    Zoned { local: NaiveDateTime, tz: Tz },
    /// Neither `Z` nor TZID. Interpreted as UTC — matching the observed TS
    /// behavior under the container/test `TZ=UTC`, but host-independent here.
    Floating(NaiveDateTime),
}

impl IcsDateTime {
    pub fn is_date(&self) -> bool {
        matches!(self, IcsDateTime::Date(_))
    }
}

#[derive(Debug, Clone)]
pub struct ParsedEvent {
    pub uid: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    /// X-MICROSOFT-CDO-BUSYSTATUS raw value.
    pub busy_status_raw: Option<String>,
    /// X-MICROSOFT-SKYPETEAMSMEETINGURL raw value.
    pub teams_url_prop: Option<String>,
    pub dtstart: IcsDateTime,
    pub dtend: Option<IcsDateTime>,
    /// Raw RRULE value string, handed to the rrule crate untouched (modulo
    /// the UNTIL-Z preprocessing in rrule_slots).
    pub rrule_raw: Option<String>,
    /// EXDATE values — multi-value lines and repeated lines merged.
    pub exdates: Vec<IcsDateTime>,
    pub recurrence_id: Option<IcsDateTime>,
    /// SEQUENCE (revision number), 0 when absent — a duplicate master or
    /// override only replaces one with a lower-or-equal sequence (node-ical
    /// parity).
    pub sequence: i64,
}

impl ParsedEvent {
    /// node-ical's `datetype === 'date'` — the all-day flag.
    pub fn is_all_day(&self) -> bool {
        self.dtstart.is_date()
    }
}

/// All VEVENTs sharing a UID: at most one master (no RECURRENCE-ID) plus its
/// overrides. `master: None` = orphan overrides whose master is absent from
/// the feed (Exchange emits these).
#[derive(Debug, Clone)]
pub struct EventGroup {
    pub master: Option<ParsedEvent>,
    pub overrides: Vec<ParsedEvent>,
}

/// Groups in feed order — first-wins dedup during expansion depends on it.
#[derive(Debug, Clone)]
pub struct ParsedCalendar {
    pub groups: Vec<EventGroup>,
}
