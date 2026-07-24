//! Line unfolding and Windows/Exchange→IANA TZID rewriting, applied to the
//! raw text BEFORE parsing. After this pass every TZID in the feed is a valid
//! IANA zone, which makes parsing deterministic and host-TZ independent.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::LazyLock;

use chrono_tz::Tz;
use regex::Regex;
use windows_timezones::WindowsTimezone;

/// IANA validity probe, replacing the TS `Intl.DateTimeFormat` try/catch.
/// `Tz::from_str` accepts canonical names and tzdb links but is case
/// sensitive, unlike `Intl` — a case-insensitive scan over `TZ_VARIANTS`
/// covers the difference.
pub fn is_valid_iana(name: &str) -> Option<Tz> {
    if let Ok(tz) = Tz::from_str(name) {
        return Some(tz);
    }
    chrono_tz::TZ_VARIANTS
        .iter()
        .copied()
        .find(|tz| tz.name().eq_ignore_ascii_case(name))
}

/// RFC 5545 §3.1 line unfolding: CRLF (or LF) followed by a space/tab joins
/// lines. Single left-to-right scan — exactly the TS `/\r?\n[ \t]/g` pass.
pub fn unfold_ics(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\r'
            && bytes.get(i + 1) == Some(&b'\n')
            && matches!(bytes.get(i + 2), Some(&b' ') | Some(&b'\t'))
        {
            i += 3;
            continue;
        }
        if b == b'\n' && matches!(bytes.get(i + 1), Some(&b' ') | Some(&b'\t')) {
            i += 2;
            continue;
        }
        out.push(b);
        i += 1;
    }
    // Only ASCII byte sequences were removed at ASCII boundaries, so the
    // result is valid UTF-8; the lossy fallback is unreachable in practice.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

pub struct NormalizedIcs {
    pub ics: String,
    /// Distinct TZID names that were neither valid IANA nor a known Windows
    /// name, in first-seen order.
    pub unknown: Vec<String>,
}

/// `;TZID=name` / `;TZID="name"` in a content line's parameter section. The
/// leading `;` anchors to a parameter named exactly TZID (never X-…-TZID).
static TZID_PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i);TZID=(?:"([^"]+)"|([^";:]+))"#).unwrap_or_else(|_| unreachable!())
});

/// Index of the `:` separating a content line's name+parameters from its
/// value (parameter values may themselves contain `:` only inside double
/// quotes). `None` for lines without a value separator.
pub(crate) fn value_separator_index(line: &str) -> Option<usize> {
    let mut in_quotes = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => return Some(i),
            _ => {}
        }
    }
    None
}

/// Quote-aware split (parameter values may contain the delimiter inside
/// double quotes).
pub(crate) fn split_quote_aware(s: &str, delim: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, ch) in s.char_indices() {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if ch == delim && !in_quotes {
            parts.push(&s[start..i]);
            start = i + 1;
        }
    }
    parts.push(&s[start..]);
    parts
}

struct Resolver<'a> {
    mapping: HashMap<String, String>,
    unknown: Vec<String>,
    tz_default: &'a str,
}

impl Resolver<'_> {
    fn resolve(&mut self, name: &str) -> String {
        if let Some(target) = self.mapping.get(name) {
            return target.clone();
        }
        let target = if is_valid_iana(name).is_some() {
            name.to_string()
        } else {
            match WindowsTimezone::from_str(name) {
                Ok(win) if is_valid_iana(win.tzdb_id()).is_some() => win.tzdb_id().to_string(),
                _ => {
                    self.unknown.push(name.to_string());
                    self.tz_default.to_string()
                }
            }
        };
        self.mapping.insert(name.to_string(), target.clone());
        target
    }

    fn rewrite_line(&mut self, line: &str) -> String {
        let Some(sep) = value_separator_index(line) else {
            return line.to_string();
        };
        let head = &line[..sep];
        let mut value = line[sep + 1..].to_string();
        let head = TZID_PARAM_RE
            .replace_all(head, |caps: &regex::Captures<'_>| {
                let name = caps
                    .get(1)
                    .or_else(|| caps.get(2))
                    .map(|m| m.as_str())
                    .unwrap_or("");
                format!(";TZID={}", self.resolve(name))
            })
            .into_owned();
        // The VTIMEZONE `TZID:<name>` property — the property name must be
        // exactly TZID (X-MICROSOFT-CDO-TZID etc. must stay untouched).
        let property_name = head.split(';').next().unwrap_or("");
        if property_name.eq_ignore_ascii_case("TZID") && !value.is_empty() {
            value = self.resolve(&value);
        }
        format!("{head}:{value}")
    }
}

/// Rewrite every Windows/Exchange TZID (e.g. `W. Europe Standard Time`) to
/// its IANA equivalent, and unknown TZIDs to `tz_default`. Operates line by
/// line on unfolded input, only on structural TZID positions — the `TZID=`
/// parameter (before the value separator) and the VTIMEZONE `TZID:` property.
/// Free text in DESCRIPTION/SUMMARY/… values is never touched. Idempotent;
/// preserves the original line terminators (CRLF/LF/CR).
pub fn normalize_tzids(unfolded: &str, tz_default: Tz) -> NormalizedIcs {
    let mut resolver = Resolver {
        mapping: HashMap::new(),
        unknown: Vec::new(),
        tz_default: tz_default.name(),
    };
    let mut ics = String::with_capacity(unfolded.len());
    let mut rest = unfolded;
    loop {
        match rest.find(['\r', '\n']) {
            None => {
                ics.push_str(&resolver.rewrite_line(rest));
                break;
            }
            Some(i) => {
                let (line, tail) = rest.split_at(i);
                ics.push_str(&resolver.rewrite_line(line));
                let term_len = if tail.starts_with("\r\n") { 2 } else { 1 };
                ics.push_str(&tail[..term_len]);
                rest = &tail[term_len..];
            }
        }
    }
    NormalizedIcs {
        ics,
        unknown: resolver.unknown,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const TZ: Tz = Tz::Europe__Prague;

    #[test]
    fn unfold_joins_crlf_space_continuations() {
        assert_eq!(
            unfold_ics("SUMMARY:Hello wo\r\n rld"),
            "SUMMARY:Hello world"
        );
    }

    #[test]
    fn unfold_joins_lf_tab_continuations() {
        assert_eq!(unfold_ics("SUMMARY:Hello wo\n\trld"), "SUMMARY:Hello world");
    }

    #[test]
    fn unfold_keeps_regular_line_breaks() {
        assert_eq!(unfold_ics("A:1\r\nB:2"), "A:1\r\nB:2");
    }

    #[test]
    fn accepts_iana_names() {
        assert_eq!(is_valid_iana("Europe/Prague"), Some(Tz::Europe__Prague));
        assert_eq!(is_valid_iana("UTC"), Some(Tz::UTC));
        assert!(is_valid_iana("America/New_York").is_some());
    }

    #[test]
    fn accepts_case_variants_like_intl_does() {
        assert_eq!(is_valid_iana("europe/prague"), Some(Tz::Europe__Prague));
    }

    #[test]
    fn rejects_windows_and_garbage_names() {
        assert_eq!(is_valid_iana("W. Europe Standard Time"), None);
        assert_eq!(is_valid_iana("Mars/Olympus_Mons"), None);
        assert_eq!(is_valid_iana(""), None);
    }

    #[test]
    fn rewrites_the_three_exchange_windows_names_wherever_tzid_appears() {
        let input = [
            "BEGIN:VTIMEZONE",
            "TZID:W. Europe Standard Time",
            "END:VTIMEZONE",
            "DTSTART;TZID=W. Europe Standard Time:20260302T090000",
            "DTSTART;TZID=Central Europe Standard Time:20260302T090000",
            "EXDATE;TZID=Central European Standard Time:20260309T090000,20260316T090000",
        ]
        .join("\r\n");
        let NormalizedIcs { ics, unknown } = normalize_tzids(&input, TZ);
        assert!(unknown.is_empty());
        assert!(ics.contains("TZID:Europe/Berlin"));
        assert!(ics.contains("TZID=Europe/Berlin:20260302T090000"));
        assert!(ics.contains("TZID=Europe/Budapest:20260302T090000"));
        assert!(ics.contains("TZID=Europe/Warsaw:20260309T090000,20260316T090000"));
        assert!(!ics.contains("Standard Time"));
    }

    #[test]
    fn rewrites_quoted_tzid_parameters() {
        let NormalizedIcs { ics, .. } = normalize_tzids(
            "DTSTART;TZID=\"W. Europe Standard Time\":20260302T090000",
            TZ,
        );
        assert_eq!(ics, "DTSTART;TZID=Europe/Berlin:20260302T090000");
    }

    #[test]
    fn leaves_valid_iana_tzids_untouched() {
        let input = "DTSTART;TZID=Europe/Prague:20260302T090000";
        assert_eq!(normalize_tzids(input, TZ).ics, input);
    }

    #[test]
    fn rewrites_unknown_tzids_to_the_default_zone_and_reports_them() {
        let NormalizedIcs { ics, unknown } =
            normalize_tzids("DTSTART;TZID=Foo Standard Time:20260319T090000", TZ);
        assert_eq!(ics, "DTSTART;TZID=Europe/Prague:20260319T090000");
        assert_eq!(unknown, ["Foo Standard Time"]);
    }

    #[test]
    fn never_touches_tzid_lookalike_text_inside_property_values() {
        let description = "DESCRIPTION:We must fix the bug where TZID=Pacific Standard Time \
                           entries are dropped\\, see ticket CAL-42 for details about TZID: parsing";
        let NormalizedIcs { ics, unknown } = normalize_tzids(description, TZ);
        assert_eq!(ics, description);
        assert!(unknown.is_empty());
    }

    #[test]
    fn never_touches_properties_whose_name_merely_ends_in_tzid() {
        let input = "X-MICROSOFT-CDO-TZID:57";
        let NormalizedIcs { ics, unknown } = normalize_tzids(input, TZ);
        assert_eq!(ics, input);
        assert!(unknown.is_empty());
    }

    #[test]
    fn rewrites_only_the_parameter_section_not_a_url_value_containing_colons() {
        let input = "DTSTART;TZID=W. Europe Standard Time:20260302T090000\r\nURL:https://example.com/a;TZID=fake";
        let NormalizedIcs { ics, .. } = normalize_tzids(input, TZ);
        assert_eq!(
            ics,
            "DTSTART;TZID=Europe/Berlin:20260302T090000\r\nURL:https://example.com/a;TZID=fake"
        );
    }

    #[test]
    fn is_idempotent() {
        let input = "DTSTART;TZID=W. Europe Standard Time:20260302T090000";
        let once = normalize_tzids(input, TZ).ics;
        assert_eq!(normalize_tzids(&once, TZ).ics, once);
    }
}
