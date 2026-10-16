//! iCloud Calendar (CalDAV) and Contacts (CardDAV) with an Apple ID + app-specific password.

use chrono::{DateTime, Duration, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, Resp, clip, render};
use crate::server::AppleServer;
use crate::util::parse_datetime;

const SECTION: &str = "icloud";
pub(crate) const CALDAV_ROOT: &str = "https://caldav.icloud.com/";
const CARDDAV_ROOT: &str = "https://contacts.icloud.com/";
const NS_DAV: &str = "DAV:";
const NS_CAL: &str = "urn:ietf:params:xml:ns:caldav";
const NS_CARD: &str = "urn:ietf:params:xml:ns:carddav";

// ---------- WebDAV multistatus parsing ----------

#[derive(Debug, Default, Clone)]
pub struct DavProp {
    pub name: String,
    pub text: String,
    /// Child element local names (resourcetype) or `comp` name attributes (supported-calendar-component-set)
    pub children: Vec<String>,
    /// Child <href> values (principal / home-set properties)
    pub hrefs: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct DavResponse {
    pub href: String,
    pub props: Vec<DavProp>,
}

impl DavResponse {
    pub fn prop(&self, name: &str) -> Option<&DavProp> {
        self.props.iter().find(|p| p.name == name)
    }
    pub fn text(&self, name: &str) -> String {
        self.prop(name).map(|p| p.text.clone()).unwrap_or_default()
    }
}

pub fn parse_multistatus(xml: &str) -> Result<Vec<DavResponse>, ToolError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| ToolError::new(format!("WebDAV XML parse error: {e}")))?;
    let mut out = Vec::new();
    for resp in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "response") {
        let mut r = DavResponse::default();
        for child in resp.children().filter(|c| c.is_element()) {
            match child.tag_name().name() {
                "href" => r.href = child.text().unwrap_or("").trim().to_string(),
                "propstat" => {
                    for prop in child.children().filter(|c| c.is_element() && c.tag_name().name() == "prop") {
                        for p in prop.children().filter(|c| c.is_element()) {
                            let mut dp = DavProp { name: p.tag_name().name().to_string(), ..Default::default() };
                            dp.text = p.children().filter(|c| c.is_text()).map(|c| c.text().unwrap_or("")).collect::<String>().trim().to_string();
                            for c in p.children().filter(|c| c.is_element()) {
                                let n = c.tag_name().name();
                                if n == "href" {
                                    dp.hrefs.push(c.text().unwrap_or("").trim().to_string());
                                } else if n == "comp" {
                                    dp.children.push(c.attribute("name").unwrap_or("").to_string());
                                } else {
                                    dp.children.push(n.to_string());
                                }
                            }
                            r.props.push(dp);
                        }
                    }
                }
                _ => {}
            }
        }
        out.push(r);
    }
    Ok(out)
}

pub fn resolve_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    match url::Url::parse(base).and_then(|b| b.join(href)) {
        Ok(u) => u.to_string(),
        Err(_) => format!("{}{}", base.trim_end_matches('/'), href),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ---------- iCalendar / vCard text helpers ----------

pub fn unfold_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.last_mut().unwrap().push_str(&line[1..]);
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// Split `NAME;PARAM=V;P2="a:b":value` into (NAME, params, value).
pub fn parse_prop(line: &str) -> Option<(String, Vec<(String, String)>, String)> {
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let mut name = parts.next()?.to_uppercase();
    if let Some(idx) = name.find('.') {
        // Apple "item1.EMAIL" grouping
        name = name[idx + 1..].to_string();
    }
    let params = parts
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.to_uppercase(), v.trim_matches('"').to_string()))
        })
        .collect();
    Some((name, params, value.to_string()))
}

pub fn ics_unescape(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\N", "\n").replace("\\,", ",").replace("\\;", ";").replace("\\\\", "\\")
}

pub fn ics_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n").replace('\r', "")
}

/// Fold a content line so no physical line exceeds 75 octets (RFC 5545 §3.1); continuation lines start with a space.
pub fn fold_line(line: &str) -> String {
    if line.len() <= 75 {
        return line.to_string();
    }
    let mut out = String::new();
    let mut current = String::new();
    let mut limit = 75;
    for c in line.chars() {
        if current.len() + c.len_utf8() > limit {
            out.push_str(&current);
            out.push_str("\r\n ");
            current.clear();
            limit = 74; // the leading space counts toward the 75-octet limit
        }
        current.push(c);
    }
    out.push_str(&current);
    out
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `20261016T090000Z` → `2026-10-16T09:00:00Z`; with TZID → `2026-10-16T09:00:00 Europe/Paris`; `20261016` → `2026-10-16`.
pub fn ics_time_display(value: &str, params: &[(String, String)]) -> String {
    let v = value.trim();
    if v.len() == 8 && v.chars().all(|c| c.is_ascii_digit()) {
        return format!("{}-{}-{}", &v[0..4], &v[4..6], &v[6..8]);
    }
    if v.len() >= 15 && &v[8..9] == "T" {
        let base = format!("{}-{}-{}T{}:{}:{}", &v[0..4], &v[4..6], &v[6..8], &v[9..11], &v[11..13], &v[13..15]);
        if v.ends_with('Z') {
            return format!("{base}Z");
        }
        if let Some(tz) = param(params, "TZID") {
            return format!("{base} {tz}");
        }
        return base;
    }
    v.to_string()
}

/// Parse all VEVENTs of an iCalendar document into compact JSON.
pub fn parse_vevents(ics: &str) -> Vec<Value> {
    let lines = unfold_lines(ics);
    let mut events = Vec::new();
    let mut cur: Option<serde_json::Map<String, Value>> = None;
    let mut attendees = 0;
    let mut in_alarm = false;
    for line in lines {
        if line == "BEGIN:VEVENT" {
            cur = Some(serde_json::Map::new());
            attendees = 0;
            continue;
        }
        if line == "BEGIN:VALARM" {
            in_alarm = true;
            continue;
        }
        if line == "END:VALARM" {
            in_alarm = false;
            continue;
        }
        if line == "END:VEVENT" {
            if let Some(mut m) = cur.take() {
                if attendees > 0 {
                    m.insert("attendees".into(), json!(attendees));
                }
                events.push(Value::Object(m));
            }
            continue;
        }
        let Some(m) = cur.as_mut() else { continue };
        if in_alarm {
            if let Some((name, _, value)) = parse_prop(&line) {
                if name == "TRIGGER" {
                    m.insert("alarm".into(), json!(value));
                }
            }
            continue;
        }
        let Some((name, params, value)) = parse_prop(&line) else { continue };
        match name.as_str() {
            "UID" => { m.insert("uid".into(), json!(value)); }
            "SUMMARY" => { m.insert("summary".into(), json!(ics_unescape(&value))); }
            "DTSTART" => {
                m.insert("start".into(), json!(ics_time_display(&value, &params)));
                if param(&params, "VALUE") == Some("DATE") { m.insert("all_day".into(), json!(true)); }
            }
            "DTEND" => { m.insert("end".into(), json!(ics_time_display(&value, &params))); }
            "DURATION" => { m.insert("duration".into(), json!(value)); }
            "LOCATION" => { m.insert("location".into(), json!(ics_unescape(&value))); }
            "DESCRIPTION" => { m.insert("description".into(), json!(clip(&ics_unescape(&value), 600))); }
            "RRULE" => { m.insert("rrule".into(), json!(value)); }
            "RECURRENCE-ID" => { m.insert("recurrence_id".into(), json!(ics_time_display(&value, &params))); }
            "STATUS" => { m.insert("status".into(), json!(value)); }
            "URL" => { m.insert("url".into(), json!(value)); }
            "ORGANIZER" => { m.insert("organizer".into(), json!(param(&params, "CN").map(|s| s.to_string()).unwrap_or_else(|| value.replace("mailto:", "")))); }
            "ATTENDEE" => { attendees += 1; }
            "LAST-MODIFIED" => { m.insert("last_modified".into(), json!(ics_time_display(&value, &params))); }
            _ => {}
        }
    }
    events
}

fn ics_utc(dt: &DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

pub struct EventSpec {
    pub uid: String,
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub description: Option<String>,
    pub location: Option<String>,
    pub rrule: Option<String>,
    pub url: Option<String>,
    pub alarm_minutes: Option<u32>,
}

fn dt_lines(spec_start: &DateTime<Utc>, spec_end: &DateTime<Utc>, all_day: bool) -> (String, String) {
    if all_day {
        (format!("DTSTART;VALUE=DATE:{}", spec_start.format("%Y%m%d")), format!("DTEND;VALUE=DATE:{}", spec_end.format("%Y%m%d")))
    } else {
        (format!("DTSTART:{}", ics_utc(spec_start)), format!("DTEND:{}", ics_utc(spec_end)))
    }
}

pub fn build_event_ics(spec: &EventSpec) -> String {
    let now = ics_utc(&Utc::now());
    let (ds, de) = dt_lines(&spec.start, &spec.end, spec.all_day);
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        "PRODID:-//apple-mcp//EN".into(),
        "CALSCALE:GREGORIAN".into(),
        "BEGIN:VEVENT".into(),
        format!("UID:{}", spec.uid),
        format!("DTSTAMP:{now}"),
        format!("CREATED:{now}"),
        ds,
        de,
        format!("SUMMARY:{}", ics_escape(&spec.summary)),
    ];
    if let Some(d) = &spec.description { lines.push(format!("DESCRIPTION:{}", ics_escape(d))); }
    if let Some(l) = &spec.location { lines.push(format!("LOCATION:{}", ics_escape(l))); }
    if let Some(u) = &spec.url { lines.push(format!("URL:{u}")); }
    if let Some(r) = &spec.rrule { lines.push(format!("RRULE:{}", r.trim_start_matches("RRULE:"))); }
    if let Some(mins) = spec.alarm_minutes {
        lines.push("BEGIN:VALARM".into());
        lines.push("ACTION:DISPLAY".into());
        lines.push("DESCRIPTION:Reminder".into());
        lines.push(format!("TRIGGER:-PT{mins}M"));
        lines.push("END:VALARM".into());
    }
    lines.push("END:VEVENT".into());
    lines.push("END:VCALENDAR".into());
    lines.iter().map(|l| fold_line(l)).collect::<Vec<_>>().join("\r\n") + "\r\n"
}

/// Replace (or insert) properties inside the master VEVENT of an existing iCalendar document.
/// `changes`: (PROPERTY NAME, full line without folding e.g. "SUMMARY:New"), None removes the property.
pub fn update_vevent(ics: &str, changes: &[(String, Option<String>)]) -> Result<String, ToolError> {
    let mut lines = unfold_lines(ics);
    // locate the first VEVENT without RECURRENCE-ID
    let mut begin = None;
    let mut i = 0;
    while i < lines.len() {
        if lines[i] == "BEGIN:VEVENT" {
            let end = lines[i..].iter().position(|l| l == "END:VEVENT").map(|p| p + i).ok_or_else(|| ToolError::new("malformed VEVENT"))?;
            let has_rid = lines[i..end].iter().any(|l| l.starts_with("RECURRENCE-ID"));
            if !has_rid || begin.is_none() {
                begin = Some((i, end));
                if !has_rid { break; }
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    let (start, mut end) = begin.ok_or_else(|| ToolError::new("no VEVENT found in the calendar object"))?;
    for (name, new_line) in changes {
        let pos = lines[start..end].iter().position(|l| {
            let mut depth_alarm = false;
            let _ = &mut depth_alarm;
            l == name || l.starts_with(&format!("{name}:")) || l.starts_with(&format!("{name};"))
        }).map(|p| p + start);
        // skip properties living inside VALARM blocks
        let pos = pos.filter(|p| {
            let before = &lines[start..*p];
            before.iter().filter(|l| *l == "BEGIN:VALARM").count() == before.iter().filter(|l| *l == "END:VALARM").count()
        });
        match (pos, new_line) {
            (Some(p), Some(l)) => lines[p] = l.clone(),
            (Some(p), None) => { lines.remove(p); end -= 1; }
            (None, Some(l)) => { lines.insert(end, l.clone()); end += 1; }
            (None, None) => {}
        }
    }
    // bump sequence/last-modified
    let now = ics_utc(&Utc::now());
    let lm = format!("LAST-MODIFIED:{now}");
    if let Some(p) = lines[start..end].iter().position(|l| l.starts_with("LAST-MODIFIED")) { lines[p + start] = lm; } else { lines.insert(end, lm); }
    Ok(lines.iter().filter(|l| !l.is_empty()).map(|l| fold_line(l)).collect::<Vec<_>>().join("\r\n") + "\r\n")
}

/// Parse vCards (3.0/4.0) into compact JSON.
pub fn parse_vcards(text: &str) -> Vec<Value> {
    let lines = unfold_lines(text);
    let mut cards = Vec::new();
    let mut cur: Option<serde_json::Map<String, Value>> = None;
    let mut emails: Vec<Value> = Vec::new();
    let mut phones: Vec<Value> = Vec::new();
    let mut addresses: Vec<Value> = Vec::new();
    for line in lines {
        if line.eq_ignore_ascii_case("BEGIN:VCARD") {
            cur = Some(serde_json::Map::new());
            emails.clear();
            phones.clear();
            addresses.clear();
            continue;
        }
        if line.eq_ignore_ascii_case("END:VCARD") {
            if let Some(mut m) = cur.take() {
                if !emails.is_empty() { m.insert("emails".into(), Value::Array(emails.clone())); }
                if !phones.is_empty() { m.insert("phones".into(), Value::Array(phones.clone())); }
                if !addresses.is_empty() { m.insert("addresses".into(), Value::Array(addresses.clone())); }
                cards.push(Value::Object(m));
            }
            continue;
        }
        let Some(m) = cur.as_mut() else { continue };
        let Some((name, params, value)) = parse_prop(&line) else { continue };
        let types = params.iter().filter(|(k, _)| k == "TYPE").map(|(_, v)| v.to_lowercase()).filter(|v| v != "internet" && v != "voice" && v != "pref").collect::<Vec<_>>().join(",");
        match name.as_str() {
            "FN" => { m.insert("full_name".into(), json!(ics_unescape(&value))); }
            "N" => {
                let parts: Vec<&str> = value.split(';').collect();
                if parts.len() >= 2 {
                    m.insert("last_name".into(), json!(ics_unescape(parts[0])));
                    m.insert("first_name".into(), json!(ics_unescape(parts[1])));
                }
            }
            "NICKNAME" => { m.insert("nickname".into(), json!(ics_unescape(&value))); }
            "ORG" => { m.insert("organization".into(), json!(ics_unescape(value.trim_end_matches(';')))); }
            "TITLE" => { m.insert("job_title".into(), json!(ics_unescape(&value))); }
            "EMAIL" => emails.push(if types.is_empty() { json!(value) } else { json!({"type": types, "value": value}) }),
            "TEL" => phones.push(if types.is_empty() { json!(value) } else { json!({"type": types, "value": value}) }),
            "ADR" => {
                let parts: Vec<String> = value.split(';').map(ics_unescape).filter(|p| !p.trim().is_empty()).collect();
                addresses.push(json!({"type": types, "value": parts.join(", ")}));
            }
            "BDAY" => { m.insert("birthday".into(), json!(value)); }
            "NOTE" => { m.insert("note".into(), json!(clip(&ics_unescape(&value), 300))); }
            "URL" => { m.insert("url".into(), json!(value)); }
            "UID" => { m.insert("uid".into(), json!(value)); }
            _ => {}
        }
    }
    cards
}

fn vcard_matches(card: &Value, q: &str) -> bool {
    let hay = card.to_string().to_lowercase();
    q.split_whitespace().all(|w| hay.contains(w))
}

// ---------- server helpers ----------

impl AppleServer {
    fn icloud_creds(&self) -> Result<(String, String), ToolError> {
        let cfg = self.cfg();
        Ok((cfg.require(SECTION, "apple_id")?, cfg.require(SECTION, "app_specific_password")?))
    }

    async fn dav(&self, method: &str, url: &str, depth: Option<&str>, content_type: Option<&str>, body: Option<String>, extra: &[(&str, String)]) -> Result<Resp, ToolError> {
        let (user, pass) = self.icloud_creds()?;
        let mut req = self.http.request(http::method(method)?, url).basic_auth(&user, Some(&pass));
        if let Some(d) = depth { req = req.header("Depth", d); }
        if let Some(ct) = content_type { req = req.header("Content-Type", ct); }
        for (k, v) in extra { req = req.header(*k, v); }
        if let Some(b) = body { req = req.body(b); }
        let resp = http::send(req).await?;
        if resp.status == 401 {
            crate::bail!("iCloud rejected the credentials (HTTP 401). Use the iCloud email as icloud.apple_id and an app-specific password (account.apple.com > Sign-In and Security > App-Specific Passwords), not the Apple ID password.");
        }
        Ok(resp)
    }

    async fn dav_multistatus(&self, method: &str, url: &str, depth: &str, body: String) -> Result<Vec<DavResponse>, ToolError> {
        let resp = self.dav(method, url, Some(depth), Some("application/xml; charset=utf-8"), Some(body), &[]).await?;
        if resp.status != 207 {
            crate::bail!("{method} {url}: HTTP {} — {}", resp.status, clip(&resp.text(), 500));
        }
        parse_multistatus(&resp.text())
    }

    pub(crate) async fn principal(&self, root: &str) -> Result<String, ToolError> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#.to_string();
        let mut rs = self.dav_multistatus("PROPFIND", root, "0", body.clone()).await;
        if rs.is_err() {
            rs = self.dav_multistatus("PROPFIND", &format!("{root}.well-known/{}", if root.contains("caldav") { "caldav" } else { "carddav" }), "0", body).await;
        }
        let rs = rs?;
        let href = rs.iter().find_map(|r| r.prop("current-user-principal").and_then(|p| p.hrefs.first().cloned())).ok_or_else(|| ToolError::new("no current-user-principal in iCloud response"))?;
        Ok(resolve_url(root, &href))
    }

    async fn caldav_home(&self) -> Result<String, ToolError> {
        if let Some(h) = self.dav.lock().expect("dav").get("caldav_home").cloned() {
            return Ok(h);
        }
        let principal = self.principal(CALDAV_ROOT).await?;
        let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="{NS_DAV}" xmlns:c="{NS_CAL}"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#);
        let rs = self.dav_multistatus("PROPFIND", &principal, "0", body).await?;
        let href = rs.iter().find_map(|r| r.prop("calendar-home-set").and_then(|p| p.hrefs.first().cloned())).ok_or_else(|| ToolError::new("no calendar-home-set in iCloud response"))?;
        let home = resolve_url(&principal, &href);
        self.dav.lock().expect("dav").insert("caldav_home".into(), home.clone());
        Ok(home)
    }

    async fn carddav_book(&self) -> Result<String, ToolError> {
        if let Some(h) = self.dav.lock().expect("dav").get("carddav_book").cloned() {
            return Ok(h);
        }
        let principal = self.principal(CARDDAV_ROOT).await?;
        let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="{NS_DAV}" xmlns:card="{NS_CARD}"><d:prop><card:addressbook-home-set/></d:prop></d:propfind>"#);
        let rs = self.dav_multistatus("PROPFIND", &principal, "0", body).await?;
        let href = rs.iter().find_map(|r| r.prop("addressbook-home-set").and_then(|p| p.hrefs.first().cloned())).ok_or_else(|| ToolError::new("no addressbook-home-set in iCloud response"))?;
        let home = resolve_url(&principal, &href);
        let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="{NS_DAV}"><d:prop><d:resourcetype/><d:displayname/></d:prop></d:propfind>"#);
        let rs = self.dav_multistatus("PROPFIND", &home, "1", body).await?;
        let book = rs
            .iter()
            .find(|r| r.prop("resourcetype").map(|p| p.children.iter().any(|c| c == "addressbook")).unwrap_or(false))
            .map(|r| resolve_url(&home, &r.href))
            .unwrap_or_else(|| format!("{}card/", home.trim_end_matches('/').to_string() + "/"));
        self.dav.lock().expect("dav").insert("carddav_book".into(), book.clone());
        Ok(book)
    }

    async fn calendars(&self) -> Result<Vec<Value>, ToolError> {
        let home = self.caldav_home().await?;
        let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="{NS_DAV}" xmlns:c="{NS_CAL}" xmlns:cs="http://calendarserver.org/ns/" xmlns:apple="http://apple.com/ns/ical/"><d:prop><d:displayname/><d:resourcetype/><c:supported-calendar-component-set/><apple:calendar-color/><cs:getctag/><c:calendar-description/></d:prop></d:propfind>"#);
        let rs = self.dav_multistatus("PROPFIND", &home, "1", body).await?;
        let mut cals = Vec::new();
        for r in rs {
            let is_cal = r.prop("resourcetype").map(|p| p.children.iter().any(|c| c == "calendar")).unwrap_or(false);
            if !is_cal {
                continue;
            }
            let comps = r.prop("supported-calendar-component-set").map(|p| p.children.clone()).unwrap_or_default();
            cals.push(json!({
                "name": r.text("displayname"),
                "href": resolve_url(&home, &r.href),
                "components": comps,
                "color": r.text("calendar-color"),
                "ctag": r.text("getctag"),
            }));
        }
        Ok(cals)
    }

    fn find_calendar<'a>(cals: &'a [Value], key: &str) -> Option<&'a Value> {
        let k = key.trim().to_lowercase();
        cals.iter().find(|c| c["href"].as_str().map(|h| h.to_lowercase() == k || h.trim_end_matches('/').ends_with(k.trim_end_matches('/'))).unwrap_or(false))
            .or_else(|| cals.iter().find(|c| c["name"].as_str().map(|n| n.to_lowercase() == k).unwrap_or(false)))
            .or_else(|| cals.iter().find(|c| c["name"].as_str().map(|n| n.to_lowercase().contains(&k)).unwrap_or(false)))
    }

    async fn calendar_query(&self, cal_href: &str, start: &DateTime<Utc>, end: &DateTime<Utc>, expand: bool) -> Result<Vec<Value>, ToolError> {
        let (s, e) = (ics_utc(start), ics_utc(end));
        let data = if expand { format!(r#"<c:calendar-data><c:expand start="{s}" end="{e}"/></c:calendar-data>"#) } else { "<c:calendar-data/>".to_string() };
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="{NS_DAV}" xmlns:c="{NS_CAL}"><d:prop><d:getetag/>{data}</d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="{s}" end="{e}"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#
        );
        let rs = match self.dav_multistatus("REPORT", cal_href, "1", body).await {
            Ok(rs) => rs,
            Err(e) if expand => {
                tracing::warn!("calendar-query with expand failed ({e}); retrying without expand");
                return Box::pin(self.calendar_query(cal_href, start, end, false)).await;
            }
            Err(e) => return Err(e),
        };
        let mut events = Vec::new();
        for r in rs {
            let etag = r.text("getetag");
            let href = resolve_url(cal_href, &r.href);
            if let Some(p) = r.prop("calendar-data") {
                for mut ev in parse_vevents(&p.text) {
                    ev["href"] = json!(href);
                    if !etag.is_empty() { ev["etag"] = json!(etag); }
                    events.push(ev);
                }
            }
        }
        Ok(events)
    }

    async fn contacts_query(&self, book: &str, query: &Option<String>, limit: usize) -> Result<Vec<Value>, ToolError> {
        let filter = match query.as_deref().map(|q| q.trim()).filter(|q| !q.is_empty()) {
            Some(q) => {
                let m = format!(r#"<card:text-match collation="i;unicode-casemap" match-type="contains">{}</card:text-match>"#, xml_escape(q));
                format!(r#"<card:filter test="anyof"><card:prop-filter name="FN">{m}</card:prop-filter><card:prop-filter name="N">{m}</card:prop-filter><card:prop-filter name="EMAIL">{m}</card:prop-filter><card:prop-filter name="ORG">{m}</card:prop-filter><card:prop-filter name="TEL">{m}</card:prop-filter><card:prop-filter name="NICKNAME">{m}</card:prop-filter></card:filter>"#)
            }
            None => r#"<card:filter><card:prop-filter name="FN"/></card:filter>"#.to_string(),
        };
        let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><card:addressbook-query xmlns:d="{NS_DAV}" xmlns:card="{NS_CARD}"><d:prop><d:getetag/><card:address-data/></d:prop>{filter}</card:addressbook-query>"#);
        let mut out = Vec::new();
        match self.dav_multistatus("REPORT", book, "1", body).await {
            Ok(rs) => {
                for r in rs {
                    if let Some(p) = r.prop("address-data") {
                        for mut c in parse_vcards(&p.text) {
                            c["href"] = json!(resolve_url(book, &r.href));
                            c["etag"] = json!(r.text("getetag"));
                            out.push(c);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("addressbook-query failed ({e}); falling back to multiget");
                // list hrefs then multiget in batches, filter client-side
                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="{NS_DAV}"><d:prop><d:getetag/><d:resourcetype/></d:prop></d:propfind>"#);
                let rs = self.dav_multistatus("PROPFIND", book, "1", body).await?;
                let hrefs: Vec<String> = rs.iter().filter(|r| r.href.ends_with(".vcf")).map(|r| r.href.clone()).collect();
                for chunk in hrefs.chunks(150) {
                    let list: String = chunk.iter().map(|h| format!("<d:href>{}</d:href>", xml_escape(h))).collect();
                    let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><card:addressbook-multiget xmlns:d="{NS_DAV}" xmlns:card="{NS_CARD}"><d:prop><d:getetag/><card:address-data/></d:prop>{list}</card:addressbook-multiget>"#);
                    let rs = self.dav_multistatus("REPORT", book, "1", body).await?;
                    for r in rs {
                        if let Some(p) = r.prop("address-data") {
                            for mut c in parse_vcards(&p.text) {
                                if let Some(q) = query.as_deref() {
                                    if !vcard_matches(&c, &q.to_lowercase()) { continue; }
                                }
                                c["href"] = json!(resolve_url(book, &r.href));
                                c["etag"] = json!(r.text("getetag"));
                                out.push(c);
                            }
                        }
                    }
                    if out.len() >= limit { break; }
                }
            }
        }
        out.sort_by(|a, b| a["full_name"].as_str().unwrap_or("").to_lowercase().cmp(&b["full_name"].as_str().unwrap_or("").to_lowercase()));
        out.truncate(limit);
        Ok(out)
    }
}

// ---------- tool params ----------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EventsParams {
    /// Range start (RFC 3339 or YYYY-MM-DD; default now)
    pub start: Option<String>,
    /// Range end (default start + 7 days)
    pub end: Option<String>,
    /// Calendar name or href (default: all calendars)
    pub calendar: Option<String>,
    /// IANA timezone for naive start/end values (default UTC)
    pub timezone: Option<String>,
    /// Max events (default 100)
    pub limit: Option<u32>,
    /// Expand recurring events into instances (default true)
    pub expand: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EventCreateParams {
    /// Calendar name or href (see icloud_calendars)
    pub calendar: String,
    /// Event title
    pub summary: String,
    /// Start (RFC 3339, or YYYY-MM-DDTHH:MM in `timezone`, or YYYY-MM-DD for all-day)
    pub start: String,
    /// End (default start + 1 hour, or next day for all-day)
    pub end: Option<String>,
    /// IANA timezone for naive start/end, e.g. Europe/Paris (default UTC)
    pub timezone: Option<String>,
    /// All-day event (default: true when start is a bare date)
    pub all_day: Option<bool>,
    pub description: Option<String>,
    pub location: Option<String>,
    /// Recurrence rule, e.g. FREQ=WEEKLY;BYDAY=MO;COUNT=10
    pub rrule: Option<String>,
    /// Display alarm N minutes before start
    pub alarm_minutes: Option<u32>,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EventUpdateParams {
    /// Event href (from icloud_events)
    pub href: String,
    pub summary: Option<String>,
    /// New start (same formats as create)
    pub start: Option<String>,
    /// New end
    pub end: Option<String>,
    /// IANA timezone for naive start/end
    pub timezone: Option<String>,
    /// Switch to all-day (true) or timed (false) when changing dates
    pub all_day: Option<bool>,
    /// New description (empty string removes it)
    pub description: Option<String>,
    /// New location (empty string removes it)
    pub location: Option<String>,
    /// New recurrence rule (empty string removes it)
    pub rrule: Option<String>,
    /// ETag for optimistic concurrency (from icloud_events)
    pub etag: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EventDeleteParams {
    /// Event href (from icloud_events)
    pub href: String,
    /// ETag for optimistic concurrency
    pub etag: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContactsParams {
    /// Text to match in name, email, organization, phone (default: all contacts)
    pub query: Option<String>,
    /// Max contacts (default 50)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContactCreateParams {
    /// Display name
    pub full_name: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    /// Email addresses (first one marked preferred)
    pub emails: Option<Vec<String>>,
    /// Phone numbers
    pub phones: Option<Vec<String>>,
    pub organization: Option<String>,
    pub job_title: Option<String>,
    pub note: Option<String>,
    /// Birthday YYYY-MM-DD
    pub birthday: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContactDeleteParams {
    /// Contact href (from icloud_contacts)
    pub href: String,
    /// ETag for optimistic concurrency
    pub etag: Option<String>,
}

// ---------- tools ----------

#[tool_router(router = icloud_dav_router, vis = "pub")]
impl AppleServer {
    /// List iCloud calendars (name, href, supported components, color). Requires icloud.apple_id + app-specific password.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn icloud_calendars(&self) -> ToolResult {
        Ok(render(&json!({"calendars": self.calendars().await?})))
    }

    /// List iCloud calendar events in a date range (all calendars or one), recurring events expanded.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn icloud_events(&self, Parameters(p): Parameters<EventsParams>) -> ToolResult {
        let tz = p.timezone.as_deref();
        let start = match &p.start { Some(s) => parse_datetime(s, tz)?, None => Utc::now() };
        let end = match &p.end { Some(e) => parse_datetime(e, tz)?, None => start + Duration::days(7) };
        if end <= start {
            crate::bail!("end must be after start");
        }
        let cals = self.calendars().await?;
        let targets: Vec<Value> = match &p.calendar {
            Some(key) => vec![Self::find_calendar(&cals, key).cloned().ok_or_else(|| ToolError::new(format!("calendar `{key}` not found; available: {}", cals.iter().map(|c| c["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(", "))))?],
            None => cals.iter().filter(|c| c["components"].as_array().map(|a| a.iter().any(|x| x == "VEVENT")).unwrap_or(true)).cloned().collect(),
        };
        let mut events = Vec::new();
        for cal in &targets {
            let href = cal["href"].as_str().unwrap_or("");
            let name = cal["name"].as_str().unwrap_or("");
            match self.calendar_query(href, &start, &end, p.expand.unwrap_or(true)).await {
                Ok(evs) => {
                    for mut ev in evs {
                        ev["calendar"] = json!(name);
                        events.push(ev);
                    }
                }
                Err(e) => tracing::warn!("calendar {name}: {e}"),
            }
        }
        events.sort_by(|a, b| a["start"].as_str().unwrap_or("").cmp(b["start"].as_str().unwrap_or("")));
        let total = events.len();
        events.truncate(p.limit.unwrap_or(100) as usize);
        Ok(render(&json!({"range": {"start": start.to_rfc3339(), "end": end.to_rfc3339()}, "calendars_searched": targets.len(), "total": total, "events": events})))
    }

    /// Create an event in an iCloud calendar (timed or all-day, optional recurrence and alarm).
    #[tool(annotations(open_world_hint = true))]
    pub async fn icloud_event_create(&self, Parameters(p): Parameters<EventCreateParams>) -> ToolResult {
        let cals = self.calendars().await?;
        let cal = Self::find_calendar(&cals, &p.calendar).ok_or_else(|| ToolError::new(format!("calendar `{}` not found; available: {}", p.calendar, cals.iter().map(|c| c["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(", "))))?;
        let tz = p.timezone.as_deref();
        let all_day = p.all_day.unwrap_or(p.start.trim().len() == 10);
        let start = parse_datetime(&p.start, tz)?;
        let end = match &p.end {
            Some(e) => {
                let e = parse_datetime(e, tz)?;
                if all_day && e <= start { start + Duration::days(1) } else { e }
            }
            None => if all_day { start + Duration::days(1) } else { start + Duration::hours(1) },
        };
        if end <= start {
            crate::bail!("end must be after start");
        }
        let uid = uuid::Uuid::new_v4().to_string().to_uppercase();
        let spec = EventSpec { uid: uid.clone(), summary: p.summary.clone(), start, end, all_day, description: p.description.clone(), location: p.location.clone(), rrule: p.rrule.clone(), url: p.url.clone(), alarm_minutes: p.alarm_minutes };
        let ics = build_event_ics(&spec);
        let href = format!("{}{uid}.ics", cal["href"].as_str().unwrap_or("").trim_end_matches('/').to_string() + "/");
        let resp = self.dav("PUT", &href, None, Some("text/calendar; charset=utf-8"), Some(ics), &[("If-None-Match", "*".to_string())]).await?;
        if !resp.ok() {
            crate::bail!("PUT {href}: HTTP {} — {}", resp.status, clip(&resp.text(), 500));
        }
        Ok(render(&json!({"created": true, "href": href, "etag": resp.header("etag"), "uid": uid, "calendar": cal["name"], "start": start.to_rfc3339(), "end": end.to_rfc3339(), "all_day": all_day})))
    }

    /// Update fields of an existing iCloud event (title, dates, location, description, recurrence).
    #[tool(annotations(open_world_hint = true))]
    pub async fn icloud_event_update(&self, Parameters(p): Parameters<EventUpdateParams>) -> ToolResult {
        let resp = self.dav("GET", &p.href, None, None, None, &[]).await?;
        if !resp.ok() {
            crate::bail!("GET {}: HTTP {}", p.href, resp.status);
        }
        let current_etag = resp.header("etag");
        let ics = resp.text();
        let tz = p.timezone.as_deref();
        let mut changes: Vec<(String, Option<String>)> = Vec::new();
        if let Some(s) = &p.summary { changes.push(("SUMMARY".into(), Some(format!("SUMMARY:{}", ics_escape(s))))); }
        for (prop, val) in [("DESCRIPTION", &p.description), ("LOCATION", &p.location)] {
            if let Some(v) = val {
                changes.push((prop.into(), if v.trim().is_empty() { None } else { Some(format!("{prop}:{}", ics_escape(v))) }));
            }
        }
        if let Some(r) = &p.rrule {
            changes.push(("RRULE".into(), if r.trim().is_empty() { None } else { Some(format!("RRULE:{}", r.trim_start_matches("RRULE:"))) }));
        }
        if p.start.is_some() || p.end.is_some() {
            // derive current values to fill the missing side
            let existing = parse_vevents(&ics).into_iter().next().unwrap_or(Value::Null);
            let cur_all_day = existing["all_day"].as_bool().unwrap_or(false);
            let all_day = p.all_day.unwrap_or(cur_all_day);
            let cur_start = existing["start"].as_str().map(|s| s.split(' ').next().unwrap_or(s).to_string());
            let cur_end = existing["end"].as_str().map(|s| s.split(' ').next().unwrap_or(s).to_string());
            let start = match &p.start { Some(s) => parse_datetime(s, tz)?, None => parse_datetime(cur_start.as_deref().unwrap_or(""), tz)? };
            let end = match &p.end {
                Some(e) => parse_datetime(e, tz)?,
                None => match cur_end { Some(e) => parse_datetime(&e, tz)?, None => start + if all_day { Duration::days(1) } else { Duration::hours(1) } },
            };
            if end <= start {
                crate::bail!("end must be after start");
            }
            let (ds, de) = dt_lines(&start, &end, all_day);
            changes.push(("DTSTART".into(), Some(ds)));
            changes.push(("DTEND".into(), Some(de)));
            changes.push(("DURATION".into(), None));
        }
        if changes.is_empty() {
            crate::bail!("nothing to update");
        }
        let updated = update_vevent(&ics, &changes)?;
        let mut extra = Vec::new();
        if let Some(e) = p.etag.clone().or(current_etag) { extra.push(("If-Match", e)); }
        let resp = self.dav("PUT", &p.href, None, Some("text/calendar; charset=utf-8"), Some(updated), &extra).await?;
        if !resp.ok() {
            crate::bail!("PUT {}: HTTP {} — {}", p.href, resp.status, clip(&resp.text(), 500));
        }
        Ok(render(&json!({"updated": true, "href": p.href, "etag": resp.header("etag"), "changed": changes.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()})))
    }

    /// Delete an iCloud calendar event by href.
    #[tool(annotations(destructive_hint = true, open_world_hint = true))]
    pub async fn icloud_event_delete(&self, Parameters(p): Parameters<EventDeleteParams>) -> ToolResult {
        let mut extra = Vec::new();
        if let Some(e) = &p.etag { extra.push(("If-Match", e.clone())); }
        let resp = self.dav("DELETE", &p.href, None, None, None, &extra).await?;
        if !resp.ok() && resp.status != 404 {
            crate::bail!("DELETE {}: HTTP {} — {}", p.href, resp.status, clip(&resp.text(), 300));
        }
        Ok(format!("Deleted {} (HTTP {}).", p.href, resp.status))
    }

    /// Search or list iCloud contacts (name, emails, phones, organization, addresses, birthday) via CardDAV.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn icloud_contacts(&self, Parameters(p): Parameters<ContactsParams>) -> ToolResult {
        let book = self.carddav_book().await?;
        let limit = p.limit.unwrap_or(50).clamp(1, 1000) as usize;
        let contacts = self.contacts_query(&book, &p.query, limit).await?;
        Ok(render(&json!({"count": contacts.len(), "contacts": contacts})))
    }

    /// Create an iCloud contact (vCard) with emails, phones, organization, note, birthday.
    #[tool(annotations(open_world_hint = true))]
    pub async fn icloud_contact_create(&self, Parameters(p): Parameters<ContactCreateParams>) -> ToolResult {
        let book = self.carddav_book().await?;
        let uid = uuid::Uuid::new_v4().to_string().to_uppercase();
        let mut lines = vec!["BEGIN:VCARD".to_string(), "VERSION:3.0".into(), "PRODID:-//apple-mcp//EN".into(), format!("UID:{uid}"), format!("FN:{}", ics_escape(&p.full_name))];
        let (first, last) = match (&p.first_name, &p.last_name) {
            (None, None) => {
                let mut it = p.full_name.trim().splitn(2, ' ');
                (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string())
            }
            (f, l) => (f.clone().unwrap_or_default(), l.clone().unwrap_or_default()),
        };
        lines.push(format!("N:{};{};;;", ics_escape(&last), ics_escape(&first)));
        if let Some(o) = &p.organization { lines.push(format!("ORG:{};", ics_escape(o))); }
        if let Some(t) = &p.job_title { lines.push(format!("TITLE:{}", ics_escape(t))); }
        for (i, e) in p.emails.clone().unwrap_or_default().iter().enumerate() {
            lines.push(format!("EMAIL;TYPE=INTERNET;TYPE=HOME{}:{}", if i == 0 { ";TYPE=pref" } else { "" }, e.trim()));
        }
        for (i, t) in p.phones.clone().unwrap_or_default().iter().enumerate() {
            lines.push(format!("TEL;TYPE=CELL;TYPE=VOICE{}:{}", if i == 0 { ";TYPE=pref" } else { "" }, t.trim()));
        }
        if let Some(b) = &p.birthday { lines.push(format!("BDAY:{}", b.trim())); }
        if let Some(n) = &p.note { lines.push(format!("NOTE:{}", ics_escape(n))); }
        if let Some(u) = &p.url { lines.push(format!("URL:{u}")); }
        lines.push(format!("REV:{}", ics_utc(&Utc::now())));
        lines.push("END:VCARD".into());
        let vcard = lines.iter().map(|l| fold_line(l)).collect::<Vec<_>>().join("\r\n") + "\r\n";
        let href = format!("{}{uid}.vcf", book.trim_end_matches('/').to_string() + "/");
        let resp = self.dav("PUT", &href, None, Some("text/vcard; charset=utf-8"), Some(vcard), &[("If-None-Match", "*".to_string())]).await?;
        if !resp.ok() {
            crate::bail!("PUT {href}: HTTP {} — {}", resp.status, clip(&resp.text(), 500));
        }
        Ok(render(&json!({"created": true, "href": href, "etag": resp.header("etag"), "uid": uid})))
    }

    /// Delete an iCloud contact by href.
    #[tool(annotations(destructive_hint = true, open_world_hint = true))]
    pub async fn icloud_contact_delete(&self, Parameters(p): Parameters<ContactDeleteParams>) -> ToolResult {
        let mut extra = Vec::new();
        if let Some(e) = &p.etag { extra.push(("If-Match", e.clone())); }
        let resp = self.dav("DELETE", &p.href, None, None, None, &extra).await?;
        if !resp.ok() && resp.status != 404 {
            crate::bail!("DELETE {}: HTTP {} — {}", p.href, resp.status, clip(&resp.text(), 300));
        }
        Ok(format!("Deleted {} (HTTP {}).", p.href, resp.status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/" xmlns:A="http://apple.com/ns/ical/">
 <response><href>/123/calendars/</href><propstat><prop><resourcetype><collection/></resourcetype><displayname>Home</displayname></prop><status>HTTP/1.1 200 OK</status></propstat></response>
 <response><href>/123/calendars/work/</href><propstat><prop><resourcetype><collection/><C:calendar/></resourcetype><displayname>Work</displayname><C:supported-calendar-component-set><C:comp name="VEVENT"/></C:supported-calendar-component-set><A:calendar-color>#FF0000FF</A:calendar-color><CS:getctag>ct1</CS:getctag></prop><status>HTTP/1.1 200 OK</status></propstat></response>
 <response><href>/123/principal/</href><propstat><prop><current-user-principal><href>/123/principal/</href></current-user-principal><C:calendar-home-set><href>https://p1-caldav.icloud.com:443/123/calendars/</href></C:calendar-home-set></prop></propstat></response>
</multistatus>"#;

    #[test]
    fn parses_multistatus() {
        let rs = parse_multistatus(MS).unwrap();
        assert_eq!(rs.len(), 3);
        let work = &rs[1];
        assert_eq!(work.href, "/123/calendars/work/");
        assert_eq!(work.text("displayname"), "Work");
        assert!(work.prop("resourcetype").unwrap().children.contains(&"calendar".to_string()));
        assert_eq!(work.prop("supported-calendar-component-set").unwrap().children, vec!["VEVENT"]);
        assert_eq!(work.text("calendar-color"), "#FF0000FF");
        let pr = &rs[2];
        assert_eq!(pr.prop("current-user-principal").unwrap().hrefs[0], "/123/principal/");
        assert_eq!(pr.prop("calendar-home-set").unwrap().hrefs[0], "https://p1-caldav.icloud.com:443/123/calendars/");
    }

    #[test]
    fn resolves_urls() {
        assert_eq!(resolve_url("https://caldav.icloud.com/", "/123/principal/"), "https://caldav.icloud.com/123/principal/");
        assert_eq!(resolve_url("https://caldav.icloud.com/123/principal/", "https://p1-caldav.icloud.com:443/123/calendars/"), "https://p1-caldav.icloud.com:443/123/calendars/");
        assert_eq!(resolve_url("https://p1-caldav.icloud.com/123/calendars/work/", "/123/calendars/work/abc.ics"), "https://p1-caldav.icloud.com/123/calendars/work/abc.ics");
    }

    #[test]
    fn parses_props_and_events() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Team sync\\, weekly\r\nDTSTART;TZID=Europe/Paris:20261016T090000\r\nDTEND;TZID=Europe/Paris:20261016T100000\r\nLOCATION:Room 1\r\nDESCRIPTION:Line one\\nLine two\r\nRRULE:FREQ=WEEKLY\r\nATTENDEE;CN=\"A, B\":mailto:a@b\r\nBEGIN:VALARM\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:u2\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261225\r\nDTEND;VALUE=DATE:20261226\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let evs = parse_vevents(ics);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["summary"], "Team sync, weekly");
        assert_eq!(evs[0]["start"], "2026-10-16T09:00:00 Europe/Paris");
        assert_eq!(evs[0]["description"], "Line one\nLine two");
        assert_eq!(evs[0]["attendees"], 1);
        assert_eq!(evs[0]["alarm"], "-PT15M");
        assert_eq!(evs[0]["rrule"], "FREQ=WEEKLY");
        assert_eq!(evs[1]["all_day"], true);
        assert_eq!(evs[1]["start"], "2026-12-25");
        let (name, params, value) = parse_prop("item1.EMAIL;TYPE=INTERNET;TYPE=HOME:a@b.c").unwrap();
        assert_eq!(name, "EMAIL");
        assert_eq!(params.len(), 2);
        assert_eq!(value, "a@b.c");
    }

    #[test]
    fn builds_and_updates_events() {
        let start = parse_datetime("2026-10-16T09:00", Some("Europe/Paris")).unwrap();
        let spec = EventSpec { uid: "U".into(), summary: "Café; test".into(), start, end: start + Duration::hours(1), all_day: false, description: Some("a\nb".into()), location: None, rrule: Some("FREQ=DAILY;COUNT=2".into()), url: None, alarm_minutes: Some(10) };
        let ics = build_event_ics(&spec);
        assert!(ics.contains("DTSTART:20261016T070000Z\r\n"));
        assert!(ics.contains("SUMMARY:Café\\; test\r\n"));
        assert!(ics.contains("DESCRIPTION:a\\nb\r\n"));
        assert!(ics.contains("TRIGGER:-PT10M"));
        let updated = update_vevent(&ics, &[("SUMMARY".into(), Some("SUMMARY:New".into())), ("DESCRIPTION".into(), None), ("LOCATION".into(), Some("LOCATION:Paris".into()))]).unwrap();
        assert!(updated.contains("SUMMARY:New\r\n"));
        assert!(!updated.contains("DESCRIPTION:a"));
        assert!(updated.contains("LOCATION:Paris\r\n"));
        assert!(updated.contains("DESCRIPTION:Reminder")); // alarm untouched
        assert!(updated.contains("LAST-MODIFIED:"));
        let evs = parse_vevents(&updated);
        assert_eq!(evs[0]["summary"], "New");
    }

    #[test]
    fn folds_long_lines() {
        let long = format!("DESCRIPTION:{}", "é".repeat(100));
        let folded = fold_line(&long);
        for l in folded.split("\r\n") {
            assert!(l.len() <= 75, "line too long: {}", l.len());
        }
        assert!(folded.lines().count() > 1);
        assert_eq!(unfold_lines(&folded)[0], long);
    }

    #[test]
    fn parses_vcards() {
        let vcf = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:c1\r\nFN:Jane Doe\r\nN:Doe;Jane;;;\r\nORG:ACME;\r\nitem1.EMAIL;type=INTERNET;type=pref:jane@acme.com\r\nTEL;type=CELL;type=VOICE:+33600000000\r\nADR;type=HOME:;;1 rue X;Paris;;75001;France\r\nBDAY:1990-01-02\r\nEND:VCARD\r\n";
        let cards = parse_vcards(vcf);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["full_name"], "Jane Doe");
        assert_eq!(cards[0]["last_name"], "Doe");
        assert_eq!(cards[0]["organization"], "ACME");
        assert_eq!(cards[0]["emails"][0], "jane@acme.com");
        assert_eq!(cards[0]["phones"][0]["type"], "cell");
        assert_eq!(cards[0]["addresses"][0]["value"], "1 rue X, Paris, 75001, France");
        assert!(vcard_matches(&cards[0], "acme jane"));
        assert!(!vcard_matches(&cards[0], "bob"));
    }
}
