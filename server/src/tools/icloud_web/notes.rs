//! iCloud Notes through CloudKit web services (container `com.apple.notes`, zone `Notes`), as
//! the icloud.com Notes app reads them. Folders and the "recents" search index are plain
//! CloudKit records; a note body (`TextDataEncrypted`) is base64 → gzip/zlib → protobuf
//! `NoteStoreProto`, rendered here as Markdown. Reading needs the PCS consent for app `notes3`.

use std::collections::HashMap;
use std::io::Read;

use base64::prelude::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{WebErr, WebSession};
use crate::error::ToolResult;
use crate::http::render;
use crate::server::AppleServer;

const PATH: &str = "/database/1/com.apple.notes/production/private";
/// Apple's "unset" timestamp sentinel (0001-01-01).
const NULL_TS: i64 = -62135596800000;
const RECENT_KEYS: &[&str] = &[
    "TitleEncrypted",
    "SnippetEncrypted",
    "ModificationDate",
    "Deleted",
    "Folder",
    "FirstAttachmentUTIEncrypted",
    "FirstAttachmentThumbnail",
    "FirstAttachmentThumbnailOrientation",
    "Attachments",
];
const TABLE_TYPES: &[&str] = &["com.apple.notes.ICTable", "com.apple.notes.ICTable2", "com.apple.notes.CRTable"];
const MAX_TABLE_AXIS_ITEMS: usize = 512;
const MAX_TABLE_CELLS: usize = 50_000;
const DEFAULT_MAX_CHARS: usize = 20_000;

fn zone_req() -> Value {
    json!({"zoneName": "Notes"})
}

fn zone_changes() -> Value {
    json!({"zoneName": "Notes", "zoneType": "REGULAR_CUSTOM_ZONE"})
}

// ---------------------------------------------------------------------------------------------
// Minimal protobuf reader
// ---------------------------------------------------------------------------------------------

/// One decoded field: number, wire type, payload (len-delimited / fixed) and varint value.
#[derive(Debug, Clone)]
struct Field {
    num: u32,
    wt: u8,
    data: Vec<u8>,
    int: u64,
}

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

/// A parsed protobuf message: the flat list of its fields (unknown ones are kept and ignored).
#[derive(Debug, Clone, Default)]
struct Msg(Vec<Field>);

impl Msg {
    fn parse(mut data: &[u8]) -> Msg {
        let mut out = Vec::new();
        while !data.is_empty() {
            let Some(key) = read_varint(&mut data) else { break };
            let num = (key >> 3) as u32;
            let wt = (key & 7) as u8;
            match wt {
                0 => {
                    let Some(v) = read_varint(&mut data) else { break };
                    out.push(Field { num, wt, data: Vec::new(), int: v });
                }
                1 | 5 => {
                    let n = if wt == 1 { 8 } else { 4 };
                    if data.len() < n {
                        break;
                    }
                    out.push(Field { num, wt, data: data[..n].to_vec(), int: 0 });
                    data = &data[n..];
                }
                2 => {
                    let Some(len) = read_varint(&mut data) else { break };
                    let len = len as usize;
                    if data.len() < len {
                        break;
                    }
                    out.push(Field { num, wt, data: data[..len].to_vec(), int: 0 });
                    data = &data[len..];
                }
                _ => break,
            }
        }
        Msg(out)
    }

    fn find(&self, num: u32, wt: u8) -> Option<&Field> {
        self.0.iter().find(|f| f.num == num && f.wt == wt)
    }

    fn uint(&self, num: u32) -> Option<u64> {
        self.find(num, 0).map(|f| f.int)
    }

    fn int32(&self, num: u32) -> Option<i32> {
        self.uint(num).map(|v| v as i32)
    }

    fn bytes(&self, num: u32) -> Option<&[u8]> {
        self.find(num, 2).map(|f| f.data.as_slice())
    }

    fn string(&self, num: u32) -> Option<String> {
        self.bytes(num).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    fn msg(&self, num: u32) -> Option<Msg> {
        self.bytes(num).map(Msg::parse)
    }

    fn msgs(&self, num: u32) -> Vec<Msg> {
        self.0.iter().filter(|f| f.num == num && f.wt == 2).map(|f| Msg::parse(&f.data)).collect()
    }

    fn strings(&self, num: u32) -> Vec<String> {
        self.0.iter().filter(|f| f.num == num && f.wt == 2).map(|f| String::from_utf8_lossy(&f.data).into_owned()).collect()
    }

    fn all_bytes(&self, num: u32) -> Vec<Vec<u8>> {
        self.0.iter().filter(|f| f.num == num && f.wt == 2).map(|f| f.data.clone()).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Note body: decompression + NoteStoreProto
// ---------------------------------------------------------------------------------------------

fn gunzip(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(raw).read_to_end(&mut out).ok().map(|_| out)
}

/// gzip (magic 1f 8b) → zlib → raw deflate, like pyicloud's `_decompress`.
fn inflate(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        return gunzip(raw);
    }
    let mut out = Vec::new();
    if flate2::read::ZlibDecoder::new(raw).read_to_end(&mut out).is_ok() {
        return Some(out);
    }
    out.clear();
    flate2::read::DeflateDecoder::new(raw).read_to_end(&mut out).ok().map(|_| out)
}

/// Paragraph attributes of a run (`ParagraphStyle`).
#[derive(Debug, Default, Clone, PartialEq)]
struct Para {
    /// `StyleType`: 0 title, 1 heading, 2 subheading, 4 monospaced, 100/101 bullets, 102 numbered, 103 checklist.
    style: Option<u32>,
    indent: u32,
    /// `Some(done)` for checklist items.
    checked: Option<bool>,
    quote: bool,
    start: Option<i32>,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Inline {
    bold: bool,
    italic: bool,
    strike: bool,
    link: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct Run {
    /// Length in UTF-16 code units.
    length: usize,
    para: Option<Para>,
    inline: Inline,
    /// `(attachment_identifier, type_uti)` when the run is an attachment placeholder.
    attachment: Option<(String, String)>,
}

/// `Document.note`: the full text (U+FFFC where attachments sit) and its attribute runs.
#[derive(Debug, Default, Clone)]
pub struct Note {
    text: String,
    runs: Vec<Run>,
}

fn safe_link(l: &str) -> bool {
    let l = l.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:") || l.starts_with("tel:")
}

impl Run {
    fn from_msg(m: &Msg) -> Run {
        let para = m.msg(2).map(|ps| Para {
            style: ps.uint(1).map(|v| v as u32),
            indent: ps.int32(4).unwrap_or(0).max(0) as u32,
            checked: ps.msg(5).map(|c| c.uint(2) == Some(1)),
            quote: ps.uint(8) == Some(1),
            start: ps.int32(7),
        });
        let weight = m.uint(5).unwrap_or(0);
        let inline = Inline { bold: weight & 1 == 1, italic: weight & 2 == 2, strike: m.uint(7) == Some(1), link: m.string(9).filter(|l| safe_link(l)) };
        let attachment = m.msg(12).map(|a| (a.string(1).unwrap_or_default(), a.string(2).unwrap_or_default()));
        Run { length: m.uint(1).unwrap_or(0) as usize, para, inline, attachment }
    }
}

impl Note {
    fn from_msg(m: &Msg) -> Note {
        Note { text: m.string(2).unwrap_or_default(), runs: m.msgs(5).iter().map(Run::from_msg).collect() }
    }

    /// Attachment identifiers referenced by the runs, in order, de-duplicated.
    fn attachment_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (id, _) in self.runs.iter().filter_map(|r| r.attachment.as_ref()) {
            if !id.is_empty() && !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }
}

/// Parse decompressed `NoteStoreProto` bytes → `document.note`.
fn parse_note_store(bytes: &[u8]) -> Option<Note> {
    let note = Msg::parse(bytes).msg(2)?.msg(3)?;
    Some(Note::from_msg(&note))
}

/// Decode a `TextDataEncrypted` value (base64 of a compressed `NoteStoreProto`).
pub fn decode_note_body(b64: &str) -> Option<Note> {
    let raw = BASE64_STANDARD.decode(b64.trim()).ok()?;
    parse_note_store(&inflate(&raw)?)
}

// ---------------------------------------------------------------------------------------------
// Attachments (CloudKit attachment records) and tables (MergeableData)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct AttachmentRec {
    id: String,
    record_name: String,
    uti: Option<String>,
    title: Option<String>,
    url: Option<String>,
    mergeable: Option<Vec<u8>>,
}

/// Bytes of an `ENCRYPTED_BYTES` / `BYTES` field (base64), or the string of a `STRING` field.
fn field_bytes(f: &Value, key: &str) -> Option<Vec<u8>> {
    let v = &f[key];
    let s = v["value"].as_str()?;
    if v["type"] == "STRING" {
        return Some(s.as_bytes().to_vec());
    }
    BASE64_STANDARD.decode(s.trim()).ok()
}

fn field_text(f: &Value, key: &str) -> Option<String> {
    field_bytes(f, key).map(|b| String::from_utf8_lossy(&b).into_owned()).filter(|s| !s.is_empty())
}

fn first_text(f: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| field_text(f, k))
}

fn attachment_rec(r: &Value) -> AttachmentRec {
    let f = &r["fields"];
    let record_name = r["recordName"].as_str().unwrap_or_default().to_string();
    let id = first_text(f, &["AttachmentIdentifier", "attachmentIdentifier", "Identifier"]).unwrap_or_else(|| record_name.clone());
    let uti = first_text(f, &["UTI", "AttachmentUTI", "UTIEncrypted"]);
    let url = first_text(f, &["URLStringEncrypted", "URLString"]);
    let mut title = first_text(f, &["TitleEncrypted", "SummaryEncrypted", "LocalizedTitleEncrypted", "AltTextEncrypted", "TokenContentIdentifierEncrypted", "Title", "Summary", "AltText"]);
    if title.is_none() && uti.as_deref() == Some("public.url") {
        title = url.clone();
    }
    AttachmentRec { id, record_name, uti, title, url, mergeable: field_bytes(f, "MergeableDataEncrypted") }
}

#[derive(Default)]
struct Axis {
    indices: HashMap<usize, usize>,
    total: usize,
}

struct TableData {
    entries: Vec<Msg>,
    uuid_index: HashMap<Vec<u8>, usize>,
    uuids: Vec<Vec<u8>>,
}

impl TableData {
    fn entry_by_oid(&self, oid: Option<Msg>) -> Option<&Msg> {
        self.entries.get(oid?.uint(6)? as usize)
    }

    /// `custom_map.map_entry[0].value.unsigned_integer_value` → index into the UUID table.
    fn uuid_idx(&self, e: &Msg) -> Option<usize> {
        let val = e.msg(13)?.msgs(3).first()?.msg(2)?.uint(2)? as usize;
        self.uuid_index.get(self.uuids.get(val)?).copied()
    }

    fn parse_axis(&self, entry: &Msg) -> Axis {
        let mut axis = Axis::default();
        let Some(ordering) = entry.msg(16).and_then(|os| os.msg(1)) else { return axis };
        if let Some(array) = ordering.msg(1) {
            for att in array.msgs(2) {
                if let Some(idx) = att.bytes(2).and_then(|u| self.uuid_index.get(u)) {
                    axis.indices.insert(*idx, axis.total);
                    axis.total += 1;
                }
            }
        }
        if let Some(contents) = ordering.msg(2) {
            for elem in contents.msgs(1) {
                let k_idx = self.entry_by_oid(elem.msg(1)).and_then(|e| self.uuid_idx(e));
                let Some(v_idx) = self.entry_by_oid(elem.msg(2)).and_then(|e| self.uuid_idx(e)) else { continue };
                let pos = k_idx.and_then(|k| axis.indices.get(&k)).or_else(|| axis.indices.get(&v_idx)).copied().unwrap_or(0);
                axis.total = axis.total.max(pos + 1);
                axis.indices.insert(v_idx, pos);
            }
        }
        axis
    }
}

/// Reconstruct a table (rows of cell texts) from a `MergeableDataEncrypted` payload.
fn table_from_mergeable(raw: &[u8]) -> Option<Vec<Vec<String>>> {
    let payload = gunzip(raw).unwrap_or_else(|| raw.to_vec());
    let data = Msg::parse(&payload).msg(2)?.msg(3)?;
    let keys = data.strings(4);
    let types = data.strings(5);
    let uuids = data.all_bytes(6);
    let uuid_index: HashMap<Vec<u8>, usize> = uuids.iter().enumerate().map(|(i, u)| (u.clone(), i)).collect();
    let td = TableData { entries: data.msgs(3), uuid_index, uuids };
    for e in &td.entries {
        let Some(cm) = e.msg(13) else { continue };
        let type_ok = cm.uint(1).and_then(|t| types.get(t as usize)).map(|t| TABLE_TYPES.contains(&t.as_str())).unwrap_or(false);
        let map_entries = cm.msgs(3);
        let keyname = |me: &Msg| me.uint(1).and_then(|k| keys.get(k as usize)).map(String::as_str);
        let names: Vec<&str> = map_entries.iter().filter_map(keyname).collect();
        if !(type_ok || (names.contains(&"crRows") && names.contains(&"crColumns") && names.contains(&"cellColumns"))) {
            continue;
        }
        let (mut rows, mut cols, mut cell_cols) = (Axis::default(), Axis::default(), None);
        for me in &map_entries {
            let Some(target) = td.entry_by_oid(me.msg(2)) else { continue };
            match keyname(me) {
                Some("crRows") => rows = td.parse_axis(target),
                Some("crColumns") => cols = td.parse_axis(target),
                Some("cellColumns") => cell_cols = Some(target),
                _ => {}
            }
        }
        if rows.total == 0 || cols.total == 0 || rows.total > MAX_TABLE_AXIS_ITEMS || cols.total > MAX_TABLE_AXIS_ITEMS || rows.total * cols.total > MAX_TABLE_CELLS {
            continue;
        }
        let mut grid = vec![vec![String::new(); cols.total]; rows.total];
        if let Some(dict) = cell_cols.and_then(|c| c.msg(6)) {
            for col in dict.msgs(1) {
                let Some(col_pos) = td.entry_by_oid(col.msg(1)).and_then(|e| td.uuid_idx(e)).and_then(|i| cols.indices.get(&i)) else { continue };
                let Some(col_dict) = td.entry_by_oid(col.msg(2)).and_then(|e| e.msg(6)) else { continue };
                for row in col_dict.msgs(1) {
                    let Some(row_pos) = td.entry_by_oid(row.msg(1)).and_then(|e| td.uuid_idx(e)).and_then(|i| rows.indices.get(&i)) else { continue };
                    let Some(note) = td.entry_by_oid(row.msg(2)).and_then(|e| e.msg(10)) else { continue };
                    if let Some(cell) = grid.get_mut(*row_pos).and_then(|r| r.get_mut(*col_pos)) {
                        *cell = render_markdown(&Note::from_msg(&note), &HashMap::new());
                    }
                }
            }
        }
        return Some(grid);
    }
    None
}

fn table_markdown(grid: &[Vec<String>]) -> String {
    let cell = |s: &String| s.trim().replace('|', "\\|").replace('\n', "<br>");
    let mut out = Vec::new();
    for (i, row) in grid.iter().enumerate() {
        out.push(format!("| {} |", row.iter().map(cell).collect::<Vec<_>>().join(" | ")));
        if i == 0 {
            out.push(format!("|{}", " --- |".repeat(row.len())));
        }
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------------------------

struct Seg {
    text: String,
    para: Option<Para>,
    inline: Inline,
    /// Pre-rendered (attachment): no inline wrapping.
    raw: bool,
    /// Stands on its own lines (table).
    block: bool,
}

#[derive(Default)]
struct Line {
    parts: Vec<(String, Inline)>,
    para: Option<Para>,
}

enum Item {
    Line(Line),
    Block(String),
}

/// `(markdown, is_block)` for an attachment placeholder.
fn attachment_markdown(id: &str, uti_hint: &str, atts: &HashMap<String, AttachmentRec>) -> (String, bool) {
    let rec = atts.get(id);
    let uti = rec.and_then(|r| r.uti.clone()).filter(|u| !u.is_empty()).unwrap_or_else(|| uti_hint.to_string());
    let lower = uti.to_ascii_lowercase();
    if lower == "com.apple.notes.table"
        && let Some(grid) = rec.and_then(|r| r.mergeable.as_deref()).and_then(table_from_mergeable)
    {
        return (table_markdown(&grid), true);
    }
    let title = rec.and_then(|r| r.title.clone()).filter(|t| !t.trim().is_empty());
    if (lower.ends_with(".hashtag") || lower.ends_with(".calculateresult"))
        && let Some(t) = &title
    {
        return (t.clone(), false);
    }
    let mut s = format!("[attachment: {}", if uti.is_empty() { "unknown" } else { uti.as_str() });
    if let Some(t) = &title {
        s.push(' ');
        s.push_str(t.trim());
    }
    if let Some(u) = rec.and_then(|r| r.url.as_deref())
        && title.as_deref() != Some(u)
    {
        s.push(' ');
        s.push_str(u);
    }
    s.push(']');
    (s, false)
}

fn clean(text: &str) -> String {
    text.replace('\0', "\u{2400}").replace('\u{FFFC}', "")
}

fn segments(note: &Note, atts: &HashMap<String, AttachmentRec>) -> Vec<Seg> {
    let units: Vec<u16> = note.text.encode_utf16().collect();
    let plain = |text: String, para: Option<Para>, inline: Inline| Seg { text, para, inline, raw: false, block: false };
    let mut segs = Vec::new();
    let mut pos = 0usize;
    for run in &note.runs {
        let mut end = pos.saturating_add(run.length).min(units.len());
        if end > pos && end < units.len() && (0xD800..=0xDBFF).contains(&units[end - 1]) {
            end += 1;
        }
        let text = String::from_utf16_lossy(&units[pos..end]);
        pos = end;
        if let Some((id, uti)) = &run.attachment {
            let (md, block) = attachment_markdown(id, uti, atts);
            segs.push(Seg { text: md, para: run.para.clone(), inline: Inline::default(), raw: true, block });
            let tail: String = text.chars().filter(|c| *c == '\n' || *c == '\u{2028}').collect();
            if !tail.is_empty() {
                segs.push(plain(tail, run.para.clone(), Inline::default()));
            }
        } else {
            segs.push(plain(clean(&text), run.para.clone(), run.inline.clone()));
        }
    }
    if pos < units.len() {
        segs.push(plain(clean(&String::from_utf16_lossy(&units[pos..])), None, Inline::default()));
    }
    segs
}

fn wrap_inline(text: &str, st: &Inline) -> String {
    let core = text.trim();
    if core.is_empty() || *st == Inline::default() {
        return text.to_string();
    }
    let lead = &text[..text.len() - text.trim_start().len()];
    let trail = &text[text.trim_end().len()..];
    let mut s = core.to_string();
    if st.bold && st.italic {
        s = format!("***{s}***");
    } else if st.bold {
        s = format!("**{s}**");
    } else if st.italic {
        s = format!("_{s}_");
    }
    if st.strike {
        s = format!("~~{s}~~");
    }
    if let Some(l) = &st.link {
        s = format!("[{s}]({l})");
    }
    format!("{lead}{s}{trail}")
}

fn render_line(parts: &[(String, Inline)]) -> String {
    let mut merged: Vec<(String, Inline)> = Vec::new();
    for (t, st) in parts {
        match merged.last_mut() {
            Some((prev, pst)) if pst == st => prev.push_str(t),
            _ => merged.push((t.clone(), st.clone())),
        }
    }
    merged.iter().map(|(t, st)| wrap_inline(t, st)).collect()
}

/// Render a decoded note as Markdown: titles as `#`, checklists as `- [ ]`/`- [x]`, lists,
/// quotes, monospaced blocks as fences, attachments as `[attachment: type]`, tables as tables.
fn render_markdown(note: &Note, atts: &HashMap<String, AttachmentRec>) -> String {
    let mut items: Vec<Item> = Vec::new();
    let mut cur = Line::default();
    let set_para = |cur: &mut Line, seg: &Seg| {
        if seg.para.is_some() && cur.para.as_ref().map(|p| p.style.is_none()).unwrap_or(true) {
            cur.para = seg.para.clone();
        }
    };
    for seg in segments(note, atts) {
        if seg.block {
            if !cur.parts.is_empty() {
                items.push(Item::Line(std::mem::take(&mut cur)));
            }
            cur = Line::default();
            items.push(Item::Block(seg.text));
            continue;
        }
        let pieces: Vec<&str> = seg.text.split(['\n', '\u{2028}']).collect();
        let n = pieces.len();
        for (k, piece) in pieces.into_iter().enumerate() {
            // an empty last piece just means the run ended at a newline: it says nothing
            // about the next paragraph's style
            if !piece.is_empty() || k + 1 < n {
                set_para(&mut cur, &seg);
            }
            if !piece.is_empty() {
                cur.parts.push((piece.to_string(), if seg.raw { Inline::default() } else { seg.inline.clone() }));
            }
            if k + 1 < n {
                items.push(Item::Line(std::mem::take(&mut cur)));
            }
        }
    }
    items.push(Item::Line(cur));

    let mut out: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut after_block = false;
    let mut counters: HashMap<u32, i64> = HashMap::new();
    for item in items {
        match item {
            Item::Block(s) => {
                if in_code {
                    out.push("```".into());
                    in_code = false;
                }
                if out.last().map(|l| !l.is_empty()).unwrap_or(false) {
                    out.push(String::new());
                }
                out.push(s);
                out.push(String::new());
                after_block = true;
            }
            Item::Line(l) => {
                let text = render_line(&l.parts);
                let para = l.para.unwrap_or_default();
                if para.style == Some(4) {
                    if !in_code {
                        out.push("```".into());
                        in_code = true;
                    }
                    after_block = false;
                    out.push(text.trim_end().to_string());
                    continue;
                }
                if in_code {
                    out.push("```".into());
                    in_code = false;
                }
                let trimmed = text.trim_end();
                if trimmed.trim().is_empty() {
                    // the newline that ended an attachment paragraph is already the block's gap
                    if !after_block {
                        out.push(String::new());
                    }
                    after_block = false;
                    continue;
                }
                after_block = false;
                let indent = "  ".repeat(para.indent as usize);
                match para.style {
                    Some(102) => counters.retain(|k, _| *k <= para.indent),
                    _ => counters.retain(|k, _| *k < para.indent),
                }
                let line = match para.style {
                    Some(0) => format!("# {}", trimmed.trim_start()),
                    Some(1) => format!("## {}", trimmed.trim_start()),
                    Some(2) => format!("### {}", trimmed.trim_start()),
                    Some(100 | 101) => format!("{indent}- {trimmed}"),
                    Some(102) => {
                        let n = counters.entry(para.indent).or_insert_with(|| i64::from(para.start.unwrap_or(1).max(1)));
                        let s = format!("{indent}{n}. {trimmed}");
                        *n += 1;
                        s
                    }
                    Some(103) => format!("{indent}- [{}] {trimmed}", if para.checked == Some(true) { 'x' } else { ' ' }),
                    _ => trimmed.to_string(),
                };
                out.push(if para.quote { format!("> {line}") } else { line });
            }
        }
    }
    if in_code {
        out.push("```".into());
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------------------------
// CloudKit field helpers
// ---------------------------------------------------------------------------------------------

fn ts(v: &Value) -> Option<String> {
    let ms = v["value"].as_i64()?;
    if ms <= NULL_TS {
        return None;
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn reference(v: &Value) -> Option<String> {
    v["value"]["recordName"].as_str().map(String::from)
}

fn folder_of(f: &Value) -> Option<String> {
    reference(&f["Folder"]).or_else(|| f["Folders"]["value"].as_array().and_then(|a| a.first()).and_then(|r| r["recordName"].as_str().map(String::from)))
}

fn is_locked(r: &Value) -> bool {
    r["recordType"].as_str().map(|t| t.eq_ignore_ascii_case("passwordprotectednote")).unwrap_or(false)
}

fn note_summary(r: &Value, folder_names: &HashMap<String, String>) -> Value {
    let f = &r["fields"];
    let folder_id = folder_of(f);
    json!({
        "id": r["recordName"],
        "title": field_text(f, "TitleEncrypted").unwrap_or_else(|| "Untitled".into()),
        "snippet": field_text(f, "SnippetEncrypted"),
        "modified": ts(&f["ModificationDate"]).or_else(|| r["modified"]["timestamp"].as_i64().and_then(|ms| ts(&json!({"value": ms})))),
        "folder_id": folder_id,
        "folder_name": folder_id.as_ref().and_then(|id| folder_names.get(id)),
        "locked": is_locked(r),
        "deleted": f["Deleted"]["value"].as_i64().unwrap_or(0) != 0,
    })
}

fn is_record(r: &Value) -> bool {
    r["serverErrorCode"].is_null() && r["recordType"].is_string()
}

// ---------------------------------------------------------------------------------------------
// CloudKit calls
// ---------------------------------------------------------------------------------------------

impl WebSession {
    async fn notes_call(&self, path: &str, body: &Value) -> Result<Value, WebErr> {
        self.ensure_pcs("notes3").await?;
        let base = self.ws_url("ckdatabasews")?;
        let v = self.post(&format!("{base}{PATH}/{path}"), &[("remapEnums", "true"), ("getCurrentSyncToken", "true")], body).await?;
        if let Some(code) = v["serverErrorCode"].as_str() {
            return Err(WebErr::Other(format!("Notes {path}: {code} {}", v["reason"].as_str().unwrap_or(""))));
        }
        Ok(v)
    }

    /// All `Folder` records (paged `changes/zone`).
    pub async fn note_folders(&self) -> Result<Vec<Value>, WebErr> {
        let mut out = Vec::new();
        let mut sync_token: Option<String> = None;
        loop {
            let mut z = json!({"zoneID": zone_changes(), "desiredKeys": ["TitleEncrypted", "ParentFolder"], "desiredRecordTypes": ["Folder"], "reverse": false});
            if let Some(t) = &sync_token {
                z["syncToken"] = json!(t);
            }
            let v = self.notes_call("changes/zone", &json!({"zones": [z]})).await?;
            let Some(zone) = v["zones"].as_array().and_then(|a| a.first()) else { break };
            if let Some(code) = zone["serverErrorCode"].as_str() {
                let reason = zone["reason"].as_str().unwrap_or("");
                return Err(match code {
                    "THROTTLED" | "TRY_AGAIN_LATER" | "ZONE_BUSY" => WebErr::Other(format!("Notes changes/zone: {code} (rate limited; retry after {} s)", zone["retryAfter"].as_f64().unwrap_or(5.0))),
                    "AUTHENTICATION_FAILED" | "AUTHENTICATION_REQUIRED" => WebErr::AuthRequired(format!("{code} {reason}")),
                    _ => WebErr::Other(format!("Notes changes/zone: {code} {reason}")),
                });
            }
            out.extend(zone["records"].as_array().into_iter().flatten().filter(|r| r["recordType"] == "Folder").cloned());
            if !zone["moreComing"].as_bool().unwrap_or(false) {
                break;
            }
            sync_token = zone["syncToken"].as_str().map(String::from);
            if sync_token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// The `recents` search index, newest first (paged `records/query`).
    pub async fn notes_recent(&self, limit: usize) -> Result<Vec<Value>, WebErr> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        while out.len() < limit {
            let mut body = json!({
                "query": {
                    "recordType": "SearchIndexes",
                    "filterBy": [{"comparator": "EQUALS", "fieldName": "indexName", "fieldValue": {"type": "STRING", "value": "recents"}}],
                    "sortBy": [{"fieldName": "modTime", "ascending": false}]
                },
                "zoneID": zone_req(),
                "desiredKeys": RECENT_KEYS,
                "resultsLimit": (limit - out.len()).min(200),
            });
            if let Some(m) = &marker {
                body["continuationMarker"] = json!(m);
            }
            let v = self.notes_call("records/query", &body).await?;
            out.extend(v["records"].as_array().into_iter().flatten().filter(|r| is_record(r)).cloned());
            marker = v["continuationMarker"].as_str().filter(|s| !s.is_empty()).map(String::from);
            if marker.is_none() {
                break;
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// `records/lookup` for the given record names; per-record errors are left in the result.
    pub async fn notes_lookup(&self, names: &[String], desired_keys: Option<&[&str]>) -> Result<Vec<Value>, WebErr> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut body = json!({"records": names.iter().map(|n| json!({"recordName": n})).collect::<Vec<_>>(), "zoneID": zone_req()});
        if let Some(k) = desired_keys {
            body["desiredKeys"] = json!(k);
        }
        let v = self.notes_call("records/lookup", &body).await?;
        Ok(v["records"].as_array().cloned().unwrap_or_default())
    }

    /// Folder id → name, from the folder list plus one lookup for ids not in it.
    async fn note_folder_names(&self, ids: &[String], known: &HashMap<String, String>) -> HashMap<String, String> {
        let mut names = known.clone();
        let missing: Vec<String> = ids.iter().filter(|id| !names.contains_key(*id)).cloned().collect();
        if !missing.is_empty()
            && let Ok(recs) = self.notes_lookup(&missing, Some(&["TitleEncrypted"])).await
        {
            for r in recs.iter().filter(|r| is_record(r)) {
                if let (Some(id), Some(name)) = (r["recordName"].as_str(), field_text(&r["fields"], "TitleEncrypted")) {
                    names.insert(id.to_string(), name);
                }
            }
        }
        names
    }
}

fn folder_json(r: &Value, all: &[Value]) -> Value {
    let id = r["recordName"].as_str().unwrap_or_default();
    json!({
        "id": id,
        "name": field_text(&r["fields"], "TitleEncrypted"),
        "parent_id": reference(&r["fields"]["ParentFolder"]),
        "has_subfolders": all.iter().any(|o| reference(&o["fields"]["ParentFolder"]).as_deref() == Some(id)),
    })
}

fn folder_name_map(folders: &[Value]) -> HashMap<String, String> {
    folders.iter().filter_map(|r| Some((r["recordName"].as_str()?.to_string(), field_text(&r["fields"], "TitleEncrypted")?))).collect()
}

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotesRecentParams {
    /// Max notes (default 20, max 200)
    pub limit: Option<usize>,
    /// Only notes in this folder: a folder id from icloud_notes_folders, or its name (case-insensitive)
    pub folder_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoteParams {
    /// Note id (the `id` from icloud_notes_recent)
    pub note_id: String,
    /// Truncate the rendered body to this many characters (default 20000)
    pub max_chars: Option<usize>,
}

#[tool_router(router = icloud_notes_router, vis = "pub")]
impl AppleServer {
    /// List iCloud Notes folders (id, name, parent folder). Needs icloud_login first.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_notes_folders(&self) -> ToolResult {
        let s = self.web().await?;
        let folders = s.note_folders().await?;
        let out: Vec<Value> = folders.iter().map(|f| folder_json(f, &folders)).collect();
        Ok(render(&json!({"count": out.len(), "folders": out})))
    }

    /// Recently modified iCloud notes (id, title, snippet, modification date, folder), optionally restricted to one folder.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_notes_recent(&self, Parameters(p): Parameters<NotesRecentParams>) -> ToolResult {
        let s = self.web().await?;
        let limit = p.limit.unwrap_or(20).clamp(1, 200);
        let folders = s.note_folders().await?;
        let names = folder_name_map(&folders);
        let folder = match p.folder_id.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            Some(want) => {
                let lower = want.to_lowercase();
                let id = if names.contains_key(want) {
                    want.to_string()
                } else if let Some((id, _)) = names.iter().find(|(_, n)| n.to_lowercase() == lower) {
                    id.clone()
                } else {
                    let known: Vec<&str> = names.values().map(String::as_str).collect();
                    crate::bail!("no Notes folder `{want}`; folders: {}", known.join(", "));
                };
                Some(id)
            }
            None => None,
        };
        let records = match &folder {
            // no server-side folder filter exists: over-fetch recents and filter here (pyicloud: max(200, limit*5))
            Some(id) => s.notes_recent((limit * 5).max(200)).await?.into_iter().filter(|r| folder_of(&r["fields"]).as_deref() == Some(id)).collect::<Vec<_>>(),
            None => s.notes_recent(limit).await?,
        };
        let ids: Vec<String> = records.iter().filter_map(|r| folder_of(&r["fields"])).collect();
        let names = s.note_folder_names(&ids, &names).await;
        let notes: Vec<Value> = records.iter().map(|r| note_summary(r, &names)).filter(|n| n["deleted"] != true).take(limit).collect();
        Ok(render(&json!({"count": notes.len(), "folder_id": folder, "notes": notes})))
    }

    /// Read one iCloud note by id: title, folder, dates and the body rendered as Markdown (checklists, lists, tables, attachment placeholders).
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_note(&self, Parameters(p): Parameters<NoteParams>) -> ToolResult {
        let s = self.web().await?;
        let id = p.note_id.trim().to_string();
        let recs = s.notes_lookup(std::slice::from_ref(&id), None).await?;
        let Some(rec) = recs.iter().find(|r| r["recordName"] == id.as_str() && is_record(r)) else {
            crate::bail!("Note not found: {id}");
        };
        let f = &rec["fields"];
        let locked = is_locked(rec);
        let note = f["TextDataEncrypted"]["value"].as_str().and_then(decode_note_body);
        let mut att_ids: Vec<String> = f["Attachments"]["value"].as_array().into_iter().flatten().filter_map(|r| r["recordName"].as_str().map(String::from)).collect();
        for a in note.as_ref().map(Note::attachment_ids).unwrap_or_default() {
            if !att_ids.contains(&a) {
                att_ids.push(a);
            }
        }
        let mut atts: HashMap<String, AttachmentRec> = HashMap::new();
        let mut att_list: Vec<Value> = Vec::new();
        if !att_ids.is_empty() {
            for r in s.notes_lookup(&att_ids, None).await?.iter().filter(|r| is_record(r)) {
                let a = attachment_rec(r);
                att_list.push(json!({"id": a.id, "uti": a.uti, "title": a.title, "url": a.url}));
                atts.insert(a.record_name.clone(), a.clone());
                atts.insert(a.id.clone(), a);
            }
        }
        let mut body = note.as_ref().map(|n| render_markdown(n, &atts)).unwrap_or_default();
        let max = p.max_chars.unwrap_or(DEFAULT_MAX_CHARS).max(1);
        let truncated = body.chars().count() > max;
        if truncated {
            body = body.chars().take(max).collect::<String>() + "\n[... truncated ...]";
        }
        let folder_id = folder_of(f);
        let names = match &folder_id {
            Some(fid) => s.note_folder_names(std::slice::from_ref(fid), &HashMap::new()).await,
            None => HashMap::new(),
        };
        Ok(render(&json!({
            "id": id,
            "title": field_text(f, "TitleEncrypted"),
            "snippet": field_text(f, "SnippetEncrypted"),
            "folder_id": folder_id,
            "folder_name": folder_id.as_ref().and_then(|i| names.get(i)),
            "modified": ts(&f["ModificationDate"]).or_else(|| rec["modified"]["timestamp"].as_i64().and_then(|ms| ts(&json!({"value": ms})))),
            "created": rec["created"]["timestamp"].as_i64().and_then(|ms| ts(&json!({"value": ms}))),
            "locked": locked,
            "note": if locked { Some("password-protected note: the body is end-to-end encrypted and cannot be read from iCloud web") } else { None },
            "attachments": att_list,
            "truncated": truncated,
            "body": body,
        })))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    // Tiny protobuf encoder for the fixtures.
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
    fn pb_b(out: &mut Vec<u8>, field: u32, b: &[u8]) {
        varint(out, u64::from((field << 3) | 2));
        varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }
    fn msg(build: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut v = Vec::new();
        build(&mut v);
        v
    }
    fn gz(b: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    }

    struct RunSpec {
        len: u64,
        style: Option<u64>,
        done: Option<bool>,
        indent: u64,
        weight: u64,
        attachment: Option<(&'static str, &'static str)>,
    }
    fn run(len: u64) -> RunSpec {
        RunSpec { len, style: None, done: None, indent: 0, weight: 0, attachment: None }
    }
    fn encode_run(r: &RunSpec) -> Vec<u8> {
        msg(|m| {
            pb_u(m, 1, r.len);
            if r.style.is_some() || r.done.is_some() || r.indent > 0 {
                let ps = msg(|ps| {
                    if let Some(s) = r.style {
                        pb_u(ps, 1, s);
                    }
                    if r.indent > 0 {
                        pb_u(ps, 4, r.indent);
                    }
                    if let Some(d) = r.done {
                        let cl = msg(|c| {
                            pb_b(c, 1, &[1u8; 16]);
                            pb_u(c, 2, u64::from(d));
                        });
                        pb_b(ps, 5, &cl);
                    }
                });
                pb_b(m, 2, &ps);
            }
            if r.weight > 0 {
                pb_u(m, 5, r.weight);
            }
            if let Some((id, uti)) = r.attachment {
                let ai = msg(|a| {
                    pb_b(a, 1, id.as_bytes());
                    pb_b(a, 2, uti.as_bytes());
                });
                pb_b(m, 12, &ai);
            }
        })
    }
    fn note_store(text: &str, runs: &[RunSpec]) -> Vec<u8> {
        let note = msg(|n| {
            pb_b(n, 2, text.as_bytes());
            for r in runs {
                pb_b(n, 5, &encode_run(r));
            }
        });
        let doc = msg(|d| {
            pb_u(d, 2, 1);
            pb_b(d, 3, &note);
        });
        msg(|s| pb_b(s, 2, &doc))
    }

    #[test]
    fn note_body_roundtrip_markdown() {
        let text = "Shopping \u{1F600}\nBuy milk\nBuy eggs\nDone\nnotes with bold\n\u{FFFC}\nend";
        let runs = [
            RunSpec { style: Some(0), ..run(12) },
            RunSpec { style: Some(103), done: Some(false), ..run(9) },
            RunSpec { style: Some(103), done: Some(true), ..run(9) },
            RunSpec { style: Some(100), indent: 1, ..run(5) },
            run(11),
            RunSpec { weight: 1, ..run(4) },
            run(1),
            RunSpec { attachment: Some(("att-1", "public.jpeg")), ..run(2) },
            RunSpec { style: Some(2), ..run(3) },
        ];
        assert_eq!(text.encode_utf16().count(), 56);
        let bytes = note_store(text, &runs);
        let b64 = BASE64_STANDARD.encode(gz(&bytes));
        let note = decode_note_body(&b64).expect("decoded");
        assert_eq!(note.text, text);
        assert_eq!(note.runs.len(), 9);
        assert_eq!(note.attachment_ids(), vec!["att-1".to_string()]);
        let md = render_markdown(&note, &HashMap::new());
        assert_eq!(md, "# Shopping \u{1F600}\n- [ ] Buy milk\n- [x] Buy eggs\n  - Done\nnotes with **bold**\n[attachment: public.jpeg]\n### end");
        // zlib-wrapped and raw-deflate bodies decode too
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&bytes).unwrap();
        assert_eq!(decode_note_body(&BASE64_STANDARD.encode(z.finish().unwrap())).unwrap().text, text);
        let mut d = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        d.write_all(&bytes).unwrap();
        assert_eq!(decode_note_body(&BASE64_STANDARD.encode(d.finish().unwrap())).unwrap().text, text);
        assert!(decode_note_body("not base64!").is_none());
    }

    #[test]
    fn numbered_lists_quotes_and_code() {
        let text = "one\ntwo\ncode\nquoted\nlink";
        let mut runs = vec![RunSpec { style: Some(102), ..run(4) }, RunSpec { style: Some(102), ..run(4) }, RunSpec { style: Some(4), ..run(5) }];
        runs.push(run(7));
        runs.push(run(4));
        let mut note = parse_note_store(&note_store(text, &runs)).unwrap();
        note.runs[3].para = Some(Para { quote: true, ..Para::default() });
        note.runs[4].inline.link = Some("https://example.com".into());
        note.runs[4].inline.italic = true;
        assert_eq!(render_markdown(&note, &HashMap::new()), "1. one\n2. two\n```\ncode\n```\n> quoted\n[_link_](https://example.com)");
        // no runs at all: plain text
        assert_eq!(render_markdown(&Note { text: "a\n\nb".into(), runs: vec![] }, &HashMap::new()), "a\n\nb");
    }

    fn oid_index(i: u64) -> Vec<u8> {
        msg(|o| pb_u(o, 6, i))
    }
    fn dict_elem(k: u64, v: u64) -> Vec<u8> {
        msg(|e| {
            pb_b(e, 1, &oid_index(k));
            pb_b(e, 2, &oid_index(v));
        })
    }
    fn id_entry(uuid_idx: u64) -> Vec<u8> {
        msg(|e| {
            let cm = msg(|cm| {
                pb_u(cm, 1, 1);
                let me = msg(|me| {
                    pb_u(me, 1, 3);
                    pb_b(me, 2, &msg(|o| pb_u(o, 2, uuid_idx)));
                });
                pb_b(cm, 3, &me);
            });
            pb_b(e, 13, &cm);
        })
    }
    fn axis_entry(uuids: &[&[u8]], id_entries: &[u64]) -> Vec<u8> {
        msg(|e| {
            let os = msg(|os| {
                let ordering = msg(|ord| {
                    let array = msg(|arr| {
                        for (i, u) in uuids.iter().enumerate() {
                            let att = msg(|a| {
                                pb_u(a, 1, i as u64);
                                pb_b(a, 2, u);
                            });
                            pb_b(arr, 2, &att);
                        }
                    });
                    pb_b(ord, 1, &array);
                    let contents = msg(|c| {
                        for i in id_entries {
                            pb_b(c, 1, &dict_elem(*i, *i));
                        }
                    });
                    pb_b(ord, 2, &contents);
                });
                pb_b(os, 1, &ordering);
            });
            pb_b(e, 16, &os);
        })
    }
    fn dict_entry(elems: &[(u64, u64)]) -> Vec<u8> {
        msg(|e| {
            let d = msg(|d| {
                for (k, v) in elems {
                    pb_b(d, 1, &dict_elem(*k, *v));
                }
            });
            pb_b(e, 6, &d);
        })
    }
    fn note_entry(text: &str) -> Vec<u8> {
        msg(|e| {
            let n = msg(|n| {
                pb_b(n, 2, text.as_bytes());
                pb_b(n, 5, &encode_run(&run(text.len() as u64)));
            });
            pb_b(e, 10, &n);
        })
    }

    #[test]
    fn table_from_mergeable_data() {
        let r0 = [0u8; 16];
        let r1 = [1u8; 16];
        let c0 = [2u8; 16];
        let c1 = [3u8; 16];
        // entries: 0 root, 1 rows axis, 2 cols axis, 3 cellColumns, 4-7 id entries, 8-9 column dicts, 10-13 cells
        let root = msg(|e| {
            let cm = msg(|cm| {
                pb_u(cm, 1, 0);
                for (k, v) in [(0u64, 1u64), (1, 2), (2, 3)] {
                    let me = msg(|me| {
                        pb_u(me, 1, k);
                        pb_b(me, 2, &oid_index(v));
                    });
                    pb_b(cm, 3, &me);
                }
            });
            pb_b(e, 13, &cm);
        });
        let entries = vec![
            root,
            axis_entry(&[&r0, &r1], &[4, 5]),
            axis_entry(&[&c0, &c1], &[6, 7]),
            dict_entry(&[(6, 8), (7, 9)]),
            id_entry(0),
            id_entry(1),
            id_entry(2),
            id_entry(3),
            dict_entry(&[(4, 10), (5, 11)]),
            dict_entry(&[(4, 12), (5, 13)]),
            note_entry("A"),
            note_entry("C"),
            note_entry("B|x"),
            note_entry("D"),
        ];
        let data = msg(|d| {
            for e in &entries {
                pb_b(d, 3, e);
            }
            for k in ["crRows", "crColumns", "cellColumns"] {
                pb_b(d, 4, k.as_bytes());
            }
            pb_b(d, 5, b"com.apple.notes.ICTable");
            for u in [&r0, &r1, &c0, &c1] {
                pb_b(d, 6, u);
            }
        });
        let obj = msg(|o| {
            pb_u(o, 2, 2);
            pb_b(o, 3, &data);
        });
        let proto = msg(|p| pb_b(p, 2, &obj));
        let grid = table_from_mergeable(&proto).expect("table");
        assert_eq!(grid, vec![vec!["A".to_string(), "B|x".to_string()], vec!["C".to_string(), "D".to_string()]]);
        assert_eq!(table_from_mergeable(&gz(&proto)).unwrap(), grid);
        assert_eq!(table_markdown(&grid), "| A | B\\|x |\n| --- | --- |\n| C | D |");
        assert!(table_from_mergeable(b"garbage").is_none());

        // a table attachment in a note renders as a markdown block
        let mut atts = HashMap::new();
        atts.insert("tbl".to_string(), AttachmentRec { id: "tbl".into(), record_name: "rec".into(), uti: Some("com.apple.notes.table".into()), mergeable: Some(gz(&proto)), ..AttachmentRec::default() });
        let note = Note { text: "Intro\n\u{FFFC}\nAfter".into(), runs: vec![run_plain(6), Run { length: 2, attachment: Some(("tbl".into(), "com.apple.notes.table".into())), ..Run::default() }, run_plain(5)] };
        assert_eq!(render_markdown(&note, &atts), "Intro\n\n| A | B\\|x |\n| --- | --- |\n| C | D |\n\nAfter");
    }

    fn run_plain(length: usize) -> Run {
        Run { length, ..Run::default() }
    }

    #[test]
    fn attachment_placeholders_and_records() {
        let rec = json!({"recordName": "ATT1", "recordType": "Attachment", "fields": {
            "AttachmentIdentifier": {"type": "STRING", "value": "logical-1"},
            "UTIEncrypted": {"type": "ENCRYPTED_BYTES", "value": BASE64_STANDARD.encode("public.url")},
            "URLStringEncrypted": {"type": "ENCRYPTED_BYTES", "value": BASE64_STANDARD.encode("https://apple.com")},
        }});
        let a = attachment_rec(&rec);
        assert_eq!(a.id, "logical-1");
        assert_eq!(a.uti.as_deref(), Some("public.url"));
        assert_eq!(a.title.as_deref(), Some("https://apple.com"));
        let mut atts = HashMap::new();
        atts.insert(a.id.clone(), a);
        assert_eq!(attachment_markdown("logical-1", "", &atts).0, "[attachment: public.url https://apple.com]");
        assert_eq!(attachment_markdown("missing", "public.jpeg", &atts).0, "[attachment: public.jpeg]");
        assert_eq!(attachment_markdown("missing", "", &atts).0, "[attachment: unknown]");
        let tag = AttachmentRec { id: "h".into(), uti: Some("com.apple.notes.inlinetextattachment.hashtag".into()), title: Some("#todo".into()), ..AttachmentRec::default() };
        atts.insert("h".into(), tag);
        assert_eq!(attachment_markdown("h", "", &atts).0, "#todo");
    }

    #[test]
    fn summaries_folders_and_timestamps() {
        let folders = vec![
            json!({"recordName": "F1", "recordType": "Folder", "fields": {"TitleEncrypted": {"type": "ENCRYPTED_BYTES", "value": BASE64_STANDARD.encode("Work")}}}),
            json!({"recordName": "F2", "recordType": "Folder", "fields": {"TitleEncrypted": {"type": "ENCRYPTED_BYTES", "value": BASE64_STANDARD.encode("Sub")}, "ParentFolder": {"type": "REFERENCE", "value": {"recordName": "F1"}}}}),
        ];
        let f1 = folder_json(&folders[0], &folders);
        assert_eq!(f1["name"], "Work");
        assert_eq!(f1["has_subfolders"], true);
        assert_eq!(folder_json(&folders[1], &folders)["parent_id"], "F1");
        let names = folder_name_map(&folders);
        let rec = json!({"recordName": "N1", "recordType": "PasswordProtectedNote", "fields": {
            "TitleEncrypted": {"type": "ENCRYPTED_BYTES", "value": BASE64_STANDARD.encode("Hello")},
            "ModificationDate": {"type": "TIMESTAMP", "value": 1700000000000i64},
            "Folders": {"type": "REFERENCE_LIST", "value": [{"recordName": "F2"}]},
            "Deleted": {"type": "INT64", "value": 0},
        }});
        let s = note_summary(&rec, &names);
        assert_eq!(s["title"], "Hello");
        assert_eq!(s["modified"], "2023-11-14T22:13:20Z");
        assert_eq!(s["folder_name"], "Sub");
        assert_eq!(s["locked"], true);
        assert_eq!(s["deleted"], false);
        assert!(ts(&json!({"value": NULL_TS})).is_none());
        assert!(!is_record(&json!({"serverErrorCode": "NOT_FOUND", "recordName": "x"})));
    }
}
