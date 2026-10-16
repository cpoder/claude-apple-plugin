//! Small shared helpers.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};

use crate::error::ToolError;

/// Push `(key, value)` when `value` is Some.
pub fn opt(q: &mut Vec<(String, String)>, key: &str, value: &Option<String>) {
    if let Some(v) = value
        && !v.trim().is_empty() {
            q.push((key.to_string(), v.trim().to_string()));
        }
}

pub fn opt_num<T: ToString>(q: &mut Vec<(String, String)>, key: &str, value: &Option<T>) {
    if let Some(v) = value {
        q.push((key.to_string(), v.to_string()));
    }
}

pub fn opt_bool(q: &mut Vec<(String, String)>, key: &str, value: &Option<bool>) {
    if let Some(v) = value {
        q.push((key.to_string(), v.to_string()));
    }
}

/// Parse a date/time string (RFC 3339, `YYYY-MM-DDTHH:MM[:SS]`, or `YYYY-MM-DD`) as UTC.
/// Naive values are interpreted in `tz` when given (IANA name), else UTC.
pub fn parse_datetime(s: &str, tz: Option<&str>) -> Result<DateTime<Utc>, ToolError> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap()))
        .map_err(|_| ToolError::new(format!("cannot parse date/time `{s}` (use RFC 3339 like 2026-10-16T09:00:00+02:00, or YYYY-MM-DD)")))?;
    match tz {
        Some(name) if !name.trim().is_empty() => {
            let zone: chrono_tz::Tz = name.trim().parse().map_err(|_| ToolError::new(format!("unknown IANA timezone `{name}`")))?;
            zone.from_local_datetime(&naive)
                .single()
                .or_else(|| zone.from_local_datetime(&naive).earliest())
                .map(|d| d.with_timezone(&Utc))
                .ok_or_else(|| ToolError::new("ambiguous local time"))
        }
        _ => Ok(Utc.from_utc_datetime(&naive)),
    }
}

/// Strip HTML tags and decode a few common entities.
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    let mut collapsed = String::with_capacity(out.len());
    let mut last_ws = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !last_ws {
                collapsed.push(' ');
            }
            last_ws = true;
        } else {
            collapsed.push(c);
            last_ws = false;
        }
    }
    collapsed.trim().to_string()
}

/// Milliseconds-since-epoch to RFC 3339.
pub fn ms_to_iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map(|d| d.to_rfc3339()).unwrap_or_default()
}

/// Add `<key>_iso` next to numeric `*Date`/`*Time` millisecond fields (Apple StoreKit payloads).
pub fn annotate_ms_dates(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            let extra: Vec<(String, String)> = map
                .iter()
                .filter_map(|(k, val)| {
                    let n = val.as_i64()?;
                    if (k.ends_with("Date") || k.ends_with("Time") || k == "signedDate") && n > 1_000_000_000_000 {
                        Some((format!("{k}_iso"), ms_to_iso(n)))
                    } else {
                        None
                    }
                })
                .collect();
            for (k, iso) in extra {
                map.insert(k, serde_json::Value::String(iso));
            }
            for (_, child) in map.iter_mut() {
                annotate_ms_dates(child);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(annotate_ms_dates),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dates() {
        assert_eq!(parse_datetime("2026-10-16", None).unwrap().to_rfc3339(), "2026-10-16T00:00:00+00:00");
        assert_eq!(parse_datetime("2026-10-16T09:00:00+02:00", None).unwrap().to_rfc3339(), "2026-10-16T07:00:00+00:00");
        assert_eq!(parse_datetime("2026-10-16T09:00", Some("Europe/Paris")).unwrap().to_rfc3339(), "2026-10-16T07:00:00+00:00");
        assert!(parse_datetime("yesterday", None).is_err());
    }

    #[test]
    fn strips_html() {
        assert_eq!(strip_html("<p>Hello&nbsp;<b>world</b>\n\n  x</p>"), "Hello world x");
    }
}
