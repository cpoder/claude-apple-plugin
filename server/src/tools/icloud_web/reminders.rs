//! iCloud Reminders through CloudKit web services (container `com.apple.reminders`, zone
//! `Reminders`), as the icloud.com Reminders app does it. Titles and notes are Apple "topotext"
//! CRDT documents (protobuf, zlib, base64); writes carry a `ResolutionTokenMap`.

use std::io::{Read, Write};

use base64::prelude::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{WebErr, WebSession};
use crate::error::{ToolError, ToolResult};
use crate::http::render;
use crate::server::AppleServer;

const PATH: &str = "/database/1/com.apple.reminders/production/private";
/// Apple's "unset" timestamp sentinel (0001-01-01).
const NULL_TS: i64 = -62135596800000;
/// Seconds between 1970-01-01 and 2001-01-01 (Apple reference date).
const APPLE_EPOCH: f64 = 978307200.0;

fn zone() -> Value {
    json!({"zoneName": "Reminders", "zoneType": "REGULAR_CUSTOM_ZONE"})
}

// ---------------------------------------------------------------------------------------------
// Minimal protobuf encode/decode for the topotext documents
// ---------------------------------------------------------------------------------------------

fn varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn pb_u(out: &mut Vec<u8>, field: u32, v: u64) {
    varint(out, u64::from(field << 3));
    varint(out, v);
}

fn pb_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    varint(out, u64::from((field << 3) | 2));
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn char_id(replica: u32, clock: u32) -> Vec<u8> {
    let mut v = Vec::new();
    pb_u(&mut v, 1, u64::from(replica));
    pb_u(&mut v, 2, u64::from(clock));
    v
}

fn substring(cid: &[u8], length: u32, ts: &[u8], child: Option<u32>) -> Vec<u8> {
    let mut v = Vec::new();
    pb_bytes(&mut v, 1, cid);
    pb_u(&mut v, 2, u64::from(length));
    pb_bytes(&mut v, 3, ts);
    if let Some(c) = child {
        pb_u(&mut v, 5, u64::from(c));
    }
    v
}

const REPLICA_UUID: [u8; 16] = [0xd4, 0x6b, 0xca, 0xe4, 0x1b, 0x87, 0x66, 0xc1, 0x8d, 0x75, 0xef, 0xe3, 0x5c, 0x91, 0x45, 0xc3];

/// Serialized `versioned_document.Document` for a fresh text (uncompressed).
pub fn crdt_document_bytes(text: &str) -> Vec<u8> {
    let len = text.encode_utf16().count() as u32;
    let mut s = Vec::new();
    pb_bytes(&mut s, 2, text.as_bytes());
    // start sentinel → child 1
    s.extend_from_slice(&{
        let mut v = Vec::new();
        pb_bytes(&mut v, 3, &substring(&char_id(0, 0), 0, &char_id(0, 0), Some(1)));
        v
    });
    if len > 0 {
        let mut v = Vec::new();
        pb_bytes(&mut v, 3, &substring(&char_id(1, 0), len, &char_id(1, 0), Some(2)));
        s.extend_from_slice(&v);
    }
    {
        let mut v = Vec::new();
        pb_bytes(&mut v, 3, &substring(&char_id(0, 0xFFFF_FFFF), 0, &char_id(0, 0xFFFF_FFFF), None));
        s.extend_from_slice(&v);
    }
    // VectorTimestamp { clock[0] { replicaUUID, replicaClock[{clock: len}, {clock: 1}] } }
    let mut clock = Vec::new();
    pb_bytes(&mut clock, 1, &REPLICA_UUID);
    for c in [len, 1] {
        let mut rc = Vec::new();
        pb_u(&mut rc, 1, u64::from(c));
        pb_bytes(&mut clock, 2, &rc);
    }
    let mut vt = Vec::new();
    pb_bytes(&mut vt, 1, &clock);
    pb_bytes(&mut s, 4, &vt);
    if len > 0 {
        let mut run = Vec::new();
        pb_u(&mut run, 1, u64::from(len));
        pb_bytes(&mut s, 5, &run);
    }
    let mut version = Vec::new();
    pb_u(&mut version, 1, 0);
    pb_u(&mut version, 2, 0);
    pb_bytes(&mut version, 3, &s);
    let mut doc = Vec::new();
    pb_u(&mut doc, 1, 0);
    pb_bytes(&mut doc, 2, &version);
    doc
}

/// base64(zlib(Document)) as stored in `TitleDocument` / `NotesDocument`.
pub fn encode_crdt(text: &str) -> String {
    let doc = crdt_document_bytes(text);
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = enc.write_all(&doc);
    BASE64_STANDARD.encode(enc.finish().unwrap_or_default())
}

/// Iterate protobuf fields: (field number, wire type, payload bytes for len-delimited / varint value).
fn pb_fields(mut data: &[u8]) -> Vec<(u32, u8, Vec<u8>, u64)> {
    let mut out = Vec::new();
    fn read_varint(d: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let (&b, rest) = d.split_first()?;
            *d = rest;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    while !data.is_empty() {
        let Some(key) = read_varint(&mut data) else { break };
        let field = (key >> 3) as u32;
        let wt = (key & 7) as u8;
        match wt {
            0 => {
                let Some(v) = read_varint(&mut data) else { break };
                out.push((field, wt, Vec::new(), v));
            }
            1 => {
                if data.len() < 8 {
                    break;
                }
                out.push((field, wt, data[..8].to_vec(), 0));
                data = &data[8..];
            }
            2 => {
                let Some(len) = read_varint(&mut data) else { break };
                let len = len as usize;
                if data.len() < len {
                    break;
                }
                out.push((field, wt, data[..len].to_vec(), 0));
                data = &data[len..];
            }
            5 => {
                if data.len() < 4 {
                    break;
                }
                out.push((field, wt, data[..4].to_vec(), 0));
                data = &data[4..];
            }
            _ => break,
        }
    }
    out
}

/// Plain text of a `TitleDocument` / `NotesDocument` value (the cached `string` field).
pub fn decode_crdt(value: &str) -> Option<String> {
    let padded = format!("{}{}", value.trim(), "=".repeat((4 - value.trim().len() % 4) % 4));
    let raw = BASE64_STANDARD.decode(padded).ok()?;
    let mut bytes = Vec::new();
    if flate2::read::ZlibDecoder::new(&raw[..]).read_to_end(&mut bytes).is_err() {
        bytes.clear();
        if flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut bytes).is_err() {
            bytes = raw.clone();
        }
    }
    let string_of = |s: &[u8]| -> Option<String> {
        pb_fields(s).into_iter().find(|(f, wt, _, _)| *f == 2 && *wt == 2).map(|(_, _, b, _)| String::from_utf8_lossy(&b).into_owned())
    };
    // Document → version[0].data → String
    let fields = pb_fields(&bytes);
    if let Some((_, _, version, _)) = fields.iter().find(|(f, wt, _, _)| *f == 2 && *wt == 2)
        && let Some((_, _, data, _)) = pb_fields(version).into_iter().find(|(f, wt, _, _)| *f == 3 && *wt == 2)
        && let Some(s) = string_of(&data)
    {
        return Some(s);
    }
    // bare Version
    if let Some((_, _, data, _)) = fields.iter().find(|(f, wt, _, _)| *f == 3 && *wt == 2)
        && let Some(s) = string_of(data)
    {
        return Some(s);
    }
    string_of(&bytes).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------------------------

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn ts(v: &Value) -> Option<String> {
    let ms = v["value"].as_i64()?;
    if ms <= NULL_TS {
        return None;
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn int(v: &Value) -> i64 {
    v["value"].as_i64().unwrap_or(0)
}

fn string(v: &Value) -> Option<String> {
    v["value"].as_str().map(String::from)
}

fn reference(v: &Value) -> Option<String> {
    v["value"]["recordName"].as_str().map(String::from)
}

fn ts_field(ms: Option<i64>) -> Value {
    match ms {
        Some(m) => json!({"type": "TIMESTAMP", "value": m}),
        None => json!({"type": "TIMESTAMP"}),
    }
}

fn int_field(v: i64) -> Value {
    json!({"type": "INT64", "value": v})
}

fn string_field(s: Option<&str>) -> Value {
    match s {
        Some(s) => json!({"type": "STRING", "value": s}),
        None => json!({"type": "STRING"}),
    }
}

fn ref_field(name: Option<&str>) -> Value {
    match name {
        Some(n) => json!({"type": "REFERENCE", "value": {"recordName": n, "action": "VALIDATE"}}),
        None => json!({"type": "REFERENCE"}),
    }
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

fn token(counter: i64, replica: &str) -> Value {
    json!({"counter": counter, "modificationTime": chrono::Utc::now().timestamp_millis() as f64 / 1000.0 - APPLE_EPOCH, "replicaID": replica})
}

/// Fresh token map: counter 1 and a distinct replica id per field.
fn generate_token_map(fields: &[&str]) -> String {
    let mut map = Map::new();
    for f in fields {
        map.insert((*f).to_string(), token(1, &new_uuid()));
    }
    json!({"map": map}).to_string()
}

/// Merge into an existing token map: counters bumped, one shared replica id for this call.
fn merge_token_map(existing: Option<&str>, fields: &[&str]) -> String {
    let mut root: Map<String, Value> = existing.and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut map = root.get("map").and_then(|m| m.as_object().cloned()).unwrap_or_default();
    let replica = new_uuid();
    for f in fields {
        let prev = map.get(*f).and_then(|t| t["counter"].as_i64()).unwrap_or(0);
        map.insert((*f).to_string(), token(prev + 1, &replica));
    }
    root.insert("map".into(), Value::Object(map));
    Value::Object(root).to_string()
}

fn reminder_json(r: &Value) -> Value {
    let f = &r["fields"];
    let title = f["TitleDocument"]["value"].as_str().and_then(decode_crdt).filter(|s| !s.is_empty()).unwrap_or_else(|| "Untitled".into());
    let notes = f["NotesDocument"]["value"].as_str().and_then(decode_crdt).unwrap_or_default();
    json!({
        "id": r["recordName"],
        "title": title,
        "notes": notes,
        "list_id": reference(&f["List"]),
        "completed": int(&f["Completed"]) != 0,
        "completed_at": ts(&f["CompletionDate"]),
        "due": ts(&f["DueDate"]),
        "all_day": int(&f["AllDay"]) != 0,
        "time_zone": string(&f["TimeZone"]),
        "priority": int(&f["Priority"]),
        "flagged": int(&f["Flagged"]) != 0,
        "deleted": int(&f["Deleted"]) != 0,
        "parent_reminder_id": reference(&f["ParentReminder"]),
        "hashtags": f["HashtagIDs"]["value"].as_array().map(|a| a.len()).unwrap_or(0),
        "alarms": f["AlarmIDs"]["value"].as_array().map(|a| a.len()).unwrap_or(0),
        "created": ts(&f["CreationDate"]).or_else(|| r["created"]["timestamp"].as_i64().and_then(|ms| ts(&json!({"value": ms})))),
        "modified": ts(&f["LastModifiedDate"]).or_else(|| r["modified"]["timestamp"].as_i64().and_then(|ms| ts(&json!({"value": ms})))),
    })
}

fn list_json(r: &Value) -> Value {
    let f = &r["fields"];
    let ids = string(&f["ReminderIDs"]).and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).map(|v| v.len()).unwrap_or(0);
    let count = int(&f["Count"]);
    json!({
        "id": r["recordName"],
        "name": string(&f["Name"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "Untitled".into()),
        "color": string(&f["Color"]).and_then(|c| serde_json::from_str::<Value>(&c).ok()).and_then(|c| c["daHexString"].as_str().map(String::from)),
        "count": if count == 0 { ids as i64 } else { count },
        "is_group": int(&f["IsGroup"]) != 0,
        "deleted": int(&f["Deleted"]) != 0,
        "sorting_style": string(&f["SortingStyle"]),
    })
}

// ---------------------------------------------------------------------------------------------
// CloudKit calls
// ---------------------------------------------------------------------------------------------

fn check_records(v: &Value, what: &str) -> Result<(), WebErr> {
    if let Some(code) = v["serverErrorCode"].as_str() {
        return Err(WebErr::Other(format!("Reminders {what}: {code} {}", v["reason"].as_str().unwrap_or(""))));
    }
    for r in v["records"].as_array().into_iter().flatten() {
        if let Some(code) = r["serverErrorCode"].as_str() {
            if code == "NOT_FOUND" {
                return Err(WebErr::Other(format!("Reminders {what}: {} not found", r["recordName"].as_str().unwrap_or("record"))));
            }
            return Err(WebErr::Other(format!("Reminders {what}: {code} {} ({})", r["reason"].as_str().unwrap_or(""), r["recordName"].as_str().unwrap_or(""))));
        }
    }
    Ok(())
}

impl WebSession {
    async fn rem(&self, path: &str, body: &Value) -> Result<Value, WebErr> {
        self.ensure_pcs("reminders").await?;
        let base = self.ws_url("ckdatabasews")?;
        let v = self.post(&format!("{base}{PATH}/{}", path.trim_start_matches('/')), &[("remapEnums", "true"), ("getCurrentSyncToken", "true")], body).await?;
        check_records(&v, path)?;
        Ok(v)
    }

    pub async fn reminder_lists(&self) -> Result<Vec<Value>, WebErr> {
        let mut out = Vec::new();
        let mut sync_token: Option<String> = None;
        loop {
            let mut z = json!({"zoneID": zone(), "desiredRecordTypes": ["List"]});
            if let Some(t) = &sync_token {
                z["syncToken"] = json!(t);
            }
            let v = self.rem("changes/zone", &json!({"zones": [z]})).await?;
            let Some(zones) = v["zones"].as_array().filter(|a| !a.is_empty()) else { break };
            let mut more = false;
            for zn in zones {
                for r in zn["records"].as_array().into_iter().flatten() {
                    if let Some(code) = r["serverErrorCode"].as_str() {
                        return Err(WebErr::Other(format!("Reminders changes/zone: {code} {}", r["reason"].as_str().unwrap_or(""))));
                    }
                    if r["recordType"] == "List" {
                        out.push(r.clone());
                    }
                }
                if zn["moreComing"].as_bool().unwrap_or(false) {
                    more = true;
                    sync_token = zn["syncToken"].as_str().map(String::from);
                }
            }
            if !more {
                break;
            }
        }
        Ok(out)
    }

    /// Resolve a list given its record name, raw UUID or (case-insensitive) name.
    pub async fn resolve_list(&self, list: &str) -> Result<Value, WebErr> {
        let lists = self.reminder_lists().await?;
        let wanted = list.trim();
        let full = if wanted.starts_with("List/") { wanted.to_string() } else { format!("List/{wanted}") };
        if let Some(l) = lists.iter().find(|l| l["recordName"] == full) {
            return Ok(l.clone());
        }
        let lower = wanted.to_lowercase();
        if let Some(l) = lists.iter().find(|l| string(&l["fields"]["Name"]).map(|n| n.to_lowercase() == lower).unwrap_or(false)) {
            return Ok(l.clone());
        }
        let names: Vec<String> = lists.iter().filter_map(|l| string(&l["fields"]["Name"])).collect();
        Err(WebErr::Other(format!("no Reminders list `{wanted}`; lists: {}", names.join(", "))))
    }

    pub async fn reminders_of(&self, list_id: &str, include_completed: bool, limit: usize) -> Result<Vec<Value>, WebErr> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut body = json!({
                "query": {
                    "recordType": "reminderList",
                    "filterBy": [
                        {"comparator": "EQUALS", "fieldName": "List", "fieldValue": {"type": "REFERENCE", "value": {"recordName": list_id, "action": "VALIDATE"}}},
                        {"comparator": "EQUALS", "fieldName": "includeCompleted", "fieldValue": {"type": "INT64", "value": if include_completed { 1 } else { 0 }}},
                        {"comparator": "EQUALS", "fieldName": "LookupValidatingReference", "fieldValue": {"type": "INT64", "value": 1}}
                    ]
                },
                "zoneID": zone(),
                "resultsLimit": limit.clamp(1, 200),
            });
            if let Some(m) = &marker {
                body["continuationMarker"] = json!(m);
            }
            let v = self.rem("records/query", &body).await?;
            for r in v["records"].as_array().into_iter().flatten() {
                if r["recordType"] == "Reminder" && reference(&r["fields"]["List"]).as_deref() == Some(list_id) {
                    out.push(r.clone());
                }
            }
            marker = v["continuationMarker"].as_str().filter(|s| !s.is_empty()).map(String::from);
            if marker.is_none() || out.len() >= limit {
                break;
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    pub async fn reminder_lookup(&self, record_name: &str) -> Result<Value, WebErr> {
        let v = self.rem("records/lookup", &json!({"records": [{"recordName": record_name}], "zoneID": zone()})).await?;
        v["records"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["recordName"] == record_name).cloned())
            .ok_or_else(|| WebErr::Other(format!("Reminder not found: {record_name}")))
    }
}

fn reminder_name(id: &str) -> String {
    let id = id.trim();
    if id.starts_with("Reminder/") { id.to_string() } else { format!("Reminder/{id}") }
}

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RemindersParams {
    /// List id (`List/…`), or its name (case-insensitive)
    pub list: String,
    /// Include completed reminders (default false)
    pub include_completed: Option<bool>,
    /// Max reminders (default 100, max 200)
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReminderCreateParams {
    /// Target list id or name
    pub list: String,
    pub title: String,
    /// Notes (plain text)
    pub notes: Option<String>,
    /// Due date/time: RFC 3339 (`2026-10-20T09:00:00+02:00`), `YYYY-MM-DDTHH:MM`, or `YYYY-MM-DD`
    pub due: Option<String>,
    /// IANA time zone for a naive `due` (default UTC), e.g. Europe/Paris
    pub time_zone: Option<String>,
    /// 0 none, 1 high, 5 medium, 9 low
    pub priority: Option<i64>,
    pub all_day: Option<bool>,
    pub flagged: Option<bool>,
    /// Parent reminder id to create a sub-task
    pub parent_reminder_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReminderCompleteParams {
    /// Reminder id (`Reminder/<UUID>` or the UUID)
    pub reminder_id: String,
    /// true = completed (default), false = reopen
    pub completed: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReminderDeleteParams {
    /// Reminder id (`Reminder/<UUID>` or the UUID)
    pub reminder_id: String,
}

#[tool_router(router = icloud_reminders_router, vis = "pub")]
impl AppleServer {
    /// List iCloud Reminders lists (id, name, color, count). Needs icloud_login first.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_reminders_lists(&self) -> ToolResult {
        let s = self.web().await?;
        let lists: Vec<Value> = s.reminder_lists().await?.iter().map(list_json).filter(|l| l["deleted"] != true).collect();
        Ok(render(&json!({"lists": lists})))
    }

    /// List the reminders of a list (by id or name): title, notes, due date, completion, priority, flag.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_reminders(&self, Parameters(p): Parameters<RemindersParams>) -> ToolResult {
        let s = self.web().await?;
        let list = s.resolve_list(&p.list).await?;
        let list_id = list["recordName"].as_str().unwrap_or_default().to_string();
        let items: Vec<Value> = s.reminders_of(&list_id, p.include_completed.unwrap_or(false), p.limit.unwrap_or(100)).await?.iter().map(reminder_json).filter(|r| r["deleted"] != true).collect();
        Ok(render(&json!({"list": list_json(&list), "count": items.len(), "reminders": items})))
    }

    /// Create a reminder in a list (title, notes, due date, priority, all-day, flag, optional parent for a sub-task).
    #[tool]
    pub async fn icloud_reminder_create(&self, Parameters(p): Parameters<ReminderCreateParams>) -> ToolResult {
        let s = self.web().await?;
        let list = s.resolve_list(&p.list).await?;
        let list_id = list["recordName"].as_str().unwrap_or_default().to_string();
        let all_day = p.all_day.unwrap_or(false);
        let (due_ms, time_zone) = match p.due.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => {
                let naive = !(d.contains('+') || d.ends_with('Z') || d.matches('-').count() > 2);
                let dt = crate::util::parse_datetime(d, p.time_zone.as_deref())?;
                let tz = if all_day {
                    None
                } else if let Some(z) = p.time_zone.clone().filter(|z| !z.is_empty()) {
                    Some(z)
                } else if naive || d.ends_with('Z') || d.ends_with("+00:00") {
                    Some("UTC".to_string())
                } else {
                    None
                };
                (Some(dt.timestamp_millis()), tz)
            }
            None => (None, None),
        };
        let now = now_ms();
        let name = format!("Reminder/{}", new_uuid());
        let completed = false;
        let mut fields = Map::new();
        fields.insert("AllDay".into(), int_field(all_day as i64));
        fields.insert("Completed".into(), int_field(completed as i64));
        fields.insert("CompletionDate".into(), ts_field(None));
        fields.insert("CreationDate".into(), ts_field(Some(now)));
        fields.insert("Deleted".into(), int_field(0));
        fields.insert("Flagged".into(), int_field(p.flagged.unwrap_or(false) as i64));
        fields.insert("Imported".into(), int_field(0));
        fields.insert("LastModifiedDate".into(), ts_field(Some(now)));
        fields.insert("List".into(), ref_field(Some(&list_id)));
        fields.insert("NotesDocument".into(), string_field(Some(&encode_crdt(p.notes.as_deref().unwrap_or("")))));
        fields.insert("Priority".into(), int_field(p.priority.unwrap_or(0)));
        fields.insert(
            "ResolutionTokenMap".into(),
            string_field(Some(&generate_token_map(&[
                "allDay", "titleDocument", "notesDocument", "parentReminder", "priority", "icsDisplayOrder", "creationDate", "list", "flagged", "completed", "completionDate", "lastModifiedDate", "recurrenceRuleIDs", "dueDate", "timeZone",
            ]))),
        );
        fields.insert("TitleDocument".into(), string_field(Some(&encode_crdt(p.title.trim()))));
        if let Some(ms) = due_ms {
            fields.insert("DueDate".into(), ts_field(Some(ms)));
        }
        if let Some(tz) = &time_zone {
            fields.insert("TimeZone".into(), string_field(Some(tz)));
        }
        if let Some(parent) = p.parent_reminder_id.as_deref().filter(|s| !s.trim().is_empty()) {
            fields.insert("ParentReminder".into(), ref_field(Some(&reminder_name(parent))));
        }
        let body = json!({
            "operations": [{
                "operationType": "create",
                "record": {"recordName": name, "recordType": "Reminder", "fields": fields, "pluginFields": {}, "parent": {"recordName": list_id}}
            }],
            "zoneID": zone(),
        });
        s.rem("records/modify", &body).await?;
        let r = s.reminder_lookup(&name).await?;
        Ok(render(&json!({"created": reminder_json(&r), "list": list_json(&list)})))
    }

    /// Mark a reminder completed (or reopen it) by id.
    #[tool]
    pub async fn icloud_reminder_complete(&self, Parameters(p): Parameters<ReminderCompleteParams>) -> ToolResult {
        let s = self.web().await?;
        let name = reminder_name(&p.reminder_id);
        let current = s.reminder_lookup(&name).await?;
        let f = &current["fields"];
        let completed = p.completed.unwrap_or(true);
        let now = now_ms();
        let completion = if completed { Some(f["CompletionDate"]["value"].as_i64().filter(|ms| *ms > NULL_TS).unwrap_or(now)) } else { None };
        let due = f["DueDate"]["value"].as_i64().filter(|ms| *ms > NULL_TS);
        let mut fields = Map::new();
        fields.insert("Completed".into(), int_field(completed as i64));
        fields.insert("CompletionDate".into(), ts_field(completion));
        fields.insert("Priority".into(), int_field(int(&f["Priority"])));
        fields.insert("Flagged".into(), int_field(int(&f["Flagged"])));
        fields.insert("AllDay".into(), int_field(int(&f["AllDay"])));
        fields.insert("LastModifiedDate".into(), ts_field(Some(now)));
        fields.insert("DueDate".into(), ts_field(due));
        fields.insert("TimeZone".into(), string_field(string(&f["TimeZone"]).as_deref()));
        fields.insert("ParentReminder".into(), ref_field(reference(&f["ParentReminder"]).as_deref()));
        fields.insert(
            "ResolutionTokenMap".into(),
            string_field(Some(&merge_token_map(string(&f["ResolutionTokenMap"]).as_deref(), &["completed", "completionDate", "priority", "flagged", "allDay", "lastModifiedDate", "dueDate", "timeZone", "parentReminder"]))),
        );
        let body = json!({
            "operations": [{
                "operationType": "update",
                "record": {"recordName": name, "recordType": "Reminder", "fields": fields, "pluginFields": {}, "recordChangeTag": current["recordChangeTag"]}
            }],
            "zoneID": zone(),
        });
        s.rem("records/modify", &body).await?;
        let r = s.reminder_lookup(&name).await?;
        Ok(render(&json!({"updated": reminder_json(&r)})))
    }

    /// Delete a reminder (moves it out of the list; Apple keeps it as a tombstone for 30 days).
    #[tool(annotations(destructive_hint = true))]
    pub async fn icloud_reminder_delete(&self, Parameters(p): Parameters<ReminderDeleteParams>) -> ToolResult {
        let s = self.web().await?;
        let name = reminder_name(&p.reminder_id);
        let current = s.reminder_lookup(&name).await?;
        let title = reminder_json(&current)["title"].clone();
        let mut fields = Map::new();
        fields.insert("Deleted".into(), int_field(1));
        fields.insert("ResolutionTokenMap".into(), string_field(Some(&generate_token_map(&["deleted", "lastModifiedDate"]))));
        fields.insert("LastModifiedDate".into(), ts_field(Some(now_ms())));
        let body = json!({
            "operations": [{
                "operationType": "update",
                "record": {"recordName": name, "recordType": "Reminder", "fields": fields, "pluginFields": {}, "recordChangeTag": current["recordChangeTag"]}
            }],
            "zoneID": zone(),
        });
        s.rem("records/modify", &body).await?;
        Ok(render(&json!({"deleted": name, "title": title})))
    }
}

impl From<ToolError> for WebErr {
    fn from(e: ToolError) -> Self {
        WebErr::Other(e.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn crdt_documents_match_pyicloud() {
        assert_eq!(hex(&crdt_document_bytes("")), "08001250080010001a4a12001a100a040800100010001a040800100028011a160a08080010ffffffff0f10001a08080010ffffffff0f221c0a1a0a10d46bcae41b8766c18d75efe35c9145c31202080012020801");
        assert_eq!(hex(&crdt_document_bytes("Test")), "0800126a080010001a641204546573741a100a040800100010001a040800100028011a100a040801100010041a040801100028021a160a08080010ffffffff0f10001a08080010ffffffff0f221c0a1a0a10d46bcae41b8766c18d75efe35c9145c312020804120208012a020804");
        assert_eq!(hex(&crdt_document_bytes("Héllo 😀")), "08001271080010001a6b120b48c3a96c6c6f20f09f98801a100a040800100010001a040800100028011a100a040801100010081a040801100028021a160a08080010ffffffff0f10001a08080010ffffffff0f221c0a1a0a10d46bcae41b8766c18d75efe35c9145c312020808120208012a020808");
    }

    #[test]
    fn crdt_roundtrip_and_python_vector() {
        for t in ["", "Test", "Héllo 😀", "Acheter du lait\nnote"] {
            assert_eq!(decode_crdt(&encode_crdt(t)).as_deref(), Some(t), "{t}");
        }
        // base64 produced by Python zlib for "Test"
        assert_eq!(decode_crdt("eJzjYBDK4mAQYJBKEWIJSS0ukRLgYgHxgSJgWoMRLMIIFGGRAtMaTFJiXBxAuf9AwA9UB2cryXBJcQlcyT71RLo97WBv6fvHMRNdDwsxcbAAMaMWkAYA4w8XgA==").as_deref(), Some("Test"));
    }

    #[test]
    fn token_maps() {
        let g: Value = serde_json::from_str(&generate_token_map(&["a", "b"])).unwrap();
        assert_eq!(g["map"]["a"]["counter"], 1);
        assert_ne!(g["map"]["a"]["replicaID"], g["map"]["b"]["replicaID"]);
        let m: Value = serde_json::from_str(&merge_token_map(Some(&g.to_string()), &["a", "c"])).unwrap();
        assert_eq!(m["map"]["a"]["counter"], 2);
        assert_eq!(m["map"]["b"]["counter"], 1);
        assert_eq!(m["map"]["c"]["counter"], 1);
        assert_eq!(m["map"]["a"]["replicaID"], m["map"]["c"]["replicaID"]);
        assert!(merge_token_map(Some("garbage"), &["x"]).contains("\"x\""));
    }

    #[test]
    fn timestamps_and_sentinels() {
        assert!(ts(&json!({"value": -62135596800000i64})).is_none());
        assert_eq!(ts(&json!({"value": 0})).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(ts_field(None), json!({"type": "TIMESTAMP"}));
    }
}
