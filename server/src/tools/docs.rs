//! Rendering of Apple developer documentation JSON (developer.apple.com/tutorials/data/...) to Markdown.

use serde_json::Value;

use crate::http::clip;

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

/// Normalize user input to the documentation path used by the JSON API, e.g. `swiftui/view`.
pub fn normalize_path(input: &str) -> String {
    let mut p = input.trim().to_string();
    if let Some(idx) = p.find("/documentation/") {
        p = p[idx + "/documentation/".len()..].to_string();
    } else if let Some(rest) = p.strip_prefix("documentation/") {
        p = rest.to_string();
    }
    if let Some(idx) = p.find('#') {
        p.truncate(idx);
    }
    if let Some(idx) = p.find('?') {
        p.truncate(idx);
    }
    p.trim_matches('/').trim_end_matches(".json").to_lowercase()
}

/// Flatten inline content (text, codeVoice, reference, emphasis, strong, link, newTerm, inlineHead, subscript...).
pub fn inline(v: &Value, refs: &Value) -> String {
    match v {
        Value::Array(items) => items.iter().map(|i| inline(i, refs)).collect::<Vec<_>>().join(""),
        Value::Object(_) => {
            let t = s(v, "type");
            match t {
                "text" => s(v, "text").to_string(),
                "codeVoice" => format!("`{}`", s(v, "code")),
                "reference" => {
                    let id = s(v, "identifier");
                    let r = refs.get(id);
                    let title = r.map(|r| s(r, "title")).filter(|t| !t.is_empty()).map(|t| t.to_string()).unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id).to_string());
                    let url = r.map(|r| s(r, "url")).unwrap_or("");
                    if url.is_empty() { format!("`{title}`") } else { format!("[{title}]({url})") }
                }
                "emphasis" => format!("*{}*", inline(&v["inlineContent"], refs)),
                "strong" => format!("**{}**", inline(&v["inlineContent"], refs)),
                "newTerm" | "inlineHead" | "subscript" | "superscript" => inline(&v["inlineContent"], refs),
                "link" => format!("[{}]({})", inline(&v["inlineContent"], refs).trim().to_string().replace("[]", "").trim(), s(v, "destination")),
                "image" => String::new(),
                _ => {
                    if v.get("inlineContent").is_some() { inline(&v["inlineContent"], refs) } else { s(v, "text").to_string() }
                }
            }
        }
        _ => String::new(),
    }
}

/// Render block content (paragraph, heading, codeListing, lists, aside, table, termList...).
pub fn blocks(v: &Value, refs: &Value, out: &mut String, depth: usize) {
    let Some(items) = v.as_array() else { return };
    for b in items {
        match s(b, "type") {
            "heading" => {
                let level = b.get("level").and_then(|l| l.as_u64()).unwrap_or(2) as usize;
                out.push_str(&format!("\n{} {}\n", "#".repeat(level.clamp(2, 5)), s(b, "text")));
            }
            "paragraph" => {
                out.push_str(&inline(&b["inlineContent"], refs));
                out.push_str("\n\n");
            }
            "codeListing" => {
                let code = b["code"].as_array().map(|lines| lines.iter().filter_map(|l| l.as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                out.push_str(&format!("```{}\n{}\n```\n\n", s(b, "syntax"), code));
            }
            "unorderedList" | "orderedList" => {
                let ordered = s(b, "type") == "orderedList";
                if let Some(items) = b["items"].as_array() {
                    for (i, it) in items.iter().enumerate() {
                        let mut inner = String::new();
                        blocks(&it["content"], refs, &mut inner, depth + 1);
                        let marker = if ordered { format!("{}.", i + 1) } else { "-".to_string() };
                        out.push_str(&format!("{}{} {}\n", "  ".repeat(depth), marker, inner.trim().replace("\n\n", "\n")));
                    }
                    out.push('\n');
                }
            }
            "aside" => {
                let mut inner = String::new();
                blocks(&b["content"], refs, &mut inner, depth);
                out.push_str(&format!("> **{}**: {}\n\n", if s(b, "name").is_empty() { s(b, "style") } else { s(b, "name") }, inner.trim().replace('\n', "\n> ")));
            }
            "termList" => {
                if let Some(items) = b["items"].as_array() {
                    for it in items {
                        let term = inline(&it["term"]["inlineContent"], refs);
                        let mut def = String::new();
                        blocks(&it["definition"]["content"], refs, &mut def, depth + 1);
                        out.push_str(&format!("- **{}**: {}\n", term, def.trim()));
                    }
                    out.push('\n');
                }
            }
            "table" => {
                if let Some(rows) = b["rows"].as_array() {
                    for (ri, row) in rows.iter().enumerate() {
                        let cells: Vec<String> = row.as_array().map(|cs| cs.iter().map(|c| { let mut t = String::new(); blocks(c, refs, &mut t, depth + 1); t.trim().replace('\n', " ") }).collect()).unwrap_or_default();
                        out.push_str(&format!("| {} |\n", cells.join(" | ")));
                        if ri == 0 { out.push_str(&format!("|{}\n", "---|".repeat(cells.len()))); }
                    }
                    out.push('\n');
                }
            }
            "links" => {
                if let Some(items) = b["items"].as_array() {
                    for id in items {
                        if let Some(r) = refs.get(id.as_str().unwrap_or("")) {
                            out.push_str(&format!("- [{}]({})\n", s(r, "title"), s(r, "url")));
                        }
                    }
                    out.push('\n');
                }
            }
            "tabNavigator" => {
                if let Some(tabs) = b["tabs"].as_array() {
                    for t in tabs {
                        out.push_str(&format!("**{}**\n\n", s(t, "title")));
                        blocks(&t["content"], refs, out, depth);
                    }
                }
            }
            "row" => {
                if let Some(cols) = b["columns"].as_array() {
                    for c in cols { blocks(&c["content"], refs, out, depth); }
                }
            }
            "small" | "video" | "image" => {}
            _ => {
                if b.get("content").is_some() { blocks(&b["content"], refs, out, depth); }
                else if b.get("inlineContent").is_some() { out.push_str(&inline(&b["inlineContent"], refs)); out.push_str("\n\n"); }
            }
        }
    }
}

fn ref_line(id: &str, refs: &Value) -> String {
    match refs.get(id) {
        Some(r) => {
            let abs = inline(&r["abstract"], refs);
            let kind = s(r, "role");
            let mut line = format!("- {}", s(r, "title"));
            if !kind.is_empty() { line.push_str(&format!(" ({kind})")); }
            if !abs.is_empty() { line.push_str(&format!(" — {}", clip(&abs, 160))); }
            let url = s(r, "url");
            if !url.is_empty() { line.push_str(&format!(" `{url}`")); }
            line
        }
        None => format!("- {id}"),
    }
}

fn rest_params(sec: &Value, refs: &Value, out: &mut String) {
    out.push_str(&format!("\n### {}\n", s(sec, "title")));
    if let Some(items) = sec["items"].as_array() {
        for it in items {
            let ty = it["type"].as_array().map(|t| t.iter().map(|x| s(x, "text")).collect::<Vec<_>>().join("")).unwrap_or_default();
            let mut desc = String::new();
            blocks(&it["content"], refs, &mut desc, 1);
            let req = if it.get("required").and_then(|r| r.as_bool()).unwrap_or(false) { " (required)" } else { "" };
            out.push_str(&format!("- `{}`{}: {}{}\n", s(it, "name"), if ty.is_empty() { String::new() } else { format!(" `{ty}`") }, req, desc.trim().replace("\n\n", " ")));
        }
    }
}

/// Render a documentation page JSON to Markdown. `section`: all | overview | topics.
pub fn render_doc(doc: &Value, section: &str, max_chars: usize) -> String {
    let refs = &doc["references"];
    let meta = &doc["metadata"];
    let mut out = String::new();
    let title = s(meta, "title");
    let heading = s(meta, "roleHeading");
    out.push_str(&format!("# {title}{}\n", if heading.is_empty() { String::new() } else { format!(" ({heading})") }));
    if let Some(id) = doc["identifier"]["url"].as_str() {
        out.push_str(&format!("URL: https://developer.apple.com{}\n", id.find("/documentation/").map(|i| &id[i..]).unwrap_or(id)));
    }
    if let Some(platforms) = meta["platforms"].as_array() {
        let p: Vec<String> = platforms.iter().map(|p| {
            let mut t = format!("{} {}+", s(p, "name"), s(p, "introducedAt"));
            if let Some(d) = p.get("deprecatedAt").and_then(|d| d.as_str()) { t.push_str(&format!(" (deprecated {d})")); }
            if p.get("beta").and_then(|b| b.as_bool()).unwrap_or(false) { t.push_str(" (beta)"); }
            t
        }).collect();
        if !p.is_empty() { out.push_str(&format!("Availability: {}\n", p.join(", "))); }
    }
    let abs = inline(&doc["abstract"], refs);
    if !abs.is_empty() { out.push_str(&format!("\n{abs}\n")); }

    if section != "topics"
        && let Some(secs) = doc["primaryContentSections"].as_array() {
            for sec in secs {
                match s(sec, "kind") {
                    "declarations" => {
                        if let Some(decls) = sec["declarations"].as_array() {
                            for d in decls {
                                let code = d["tokens"].as_array().map(|t| t.iter().map(|x| s(x, "text")).collect::<Vec<_>>().join("")).unwrap_or_default();
                                let lang = d["languages"].as_array().and_then(|l| l.first()).and_then(|l| l.as_str()).unwrap_or("swift");
                                out.push_str(&format!("\n```{lang}\n{code}\n```\n"));
                            }
                        }
                    }
                    "content" => blocks(&sec["content"], refs, &mut out, 0),
                    "parameters" => {
                        out.push_str("\n### Parameters\n");
                        if let Some(params) = sec["parameters"].as_array() {
                            for p in params {
                                let mut d = String::new();
                                blocks(&p["content"], refs, &mut d, 1);
                                out.push_str(&format!("- `{}`: {}\n", s(p, "name"), d.trim().replace("\n\n", " ")));
                            }
                        }
                    }
                    "restEndpoint" => {
                        let ep = sec["tokens"].as_array().map(|t| t.iter().map(|x| s(x, "text")).collect::<Vec<_>>().join("")).unwrap_or_default();
                        out.push_str(&format!("\n```http\n{ep}\n```\n"));
                    }
                    "restParameters" | "restBody" => rest_params(sec, refs, &mut out),
                    "restResponses" => {
                        out.push_str("\n### Responses\n");
                        if let Some(items) = sec["items"].as_array() {
                            for it in items {
                                let ty = it["type"].as_array().map(|t| t.iter().map(|x| s(x, "text")).collect::<Vec<_>>().join("")).unwrap_or_default();
                                let mut d = String::new();
                                blocks(&it["content"], refs, &mut d, 1);
                                out.push_str(&format!("- {} {} `{}` {}\n", it["status"].as_u64().map(|n| n.to_string()).unwrap_or_default(), s(it, "reason"), ty, d.trim().replace("\n\n", " ")));
                            }
                        }
                    }
                    "properties" | "possibleValues" => {
                        out.push_str(&format!("\n### {}\n", s(sec, "title")));
                        let key = if s(sec, "kind") == "properties" { "items" } else { "values" };
                        if let Some(items) = sec[key].as_array() {
                            for it in items {
                                let ty = it["type"].as_array().map(|t| t.iter().map(|x| s(x, "text")).collect::<Vec<_>>().join("")).unwrap_or_default();
                                let mut d = String::new();
                                blocks(&it["content"], refs, &mut d, 1);
                                let req = if it.get("required").and_then(|r| r.as_bool()).unwrap_or(false) { " (required)" } else { "" };
                                out.push_str(&format!("- `{}`{}{}: {}\n", s(it, "name"), if ty.is_empty() { String::new() } else { format!(" `{ty}`") }, req, d.trim().replace("\n\n", " ")));
                            }
                        }
                    }
                    "mentions" => {}
                    other => {
                        if sec.get("content").is_some() { blocks(&sec["content"], refs, &mut out, 0); }
                        else { out.push_str(&format!("\n[section {other} omitted]\n")); }
                    }
                }
            }
        }

    if section != "overview" {
        for (key, label) in [("topicSections", "Topics"), ("relationshipsSections", "Relationships"), ("seeAlsoSections", "See Also")] {
            if let Some(secs) = doc[key].as_array() {
                if secs.is_empty() { continue; }
                out.push_str(&format!("\n## {label}\n"));
                for sec in secs {
                    out.push_str(&format!("\n### {}\n", s(sec, "title")));
                    let ids = sec["identifiers"].as_array().cloned().unwrap_or_default();
                    let shown = ids.iter().take(40);
                    for id in shown {
                        out.push_str(&ref_line(id.as_str().unwrap_or(""), refs));
                        out.push('\n');
                    }
                    if ids.len() > 40 { out.push_str(&format!("- … {} more\n", ids.len() - 40)); }
                }
            }
        }
    }
    if out.chars().count() > max_chars {
        let cut: String = out.chars().take(max_chars).collect();
        format!("{cut}\n\n[... truncated at {max_chars} chars; use section=overview|topics or raise max_chars ...]")
    } else {
        out
    }
}

/// Flatten a documentation index tree into (title, path, type) rows.
pub fn flatten_index(node: &Value, out: &mut Vec<(String, String, String)>, depth: usize, max_depth: usize) {
    if let Some(arr) = node.as_array() {
        for n in arr { flatten_index(n, out, depth, max_depth); }
        return;
    }
    let title = s(node, "title");
    let path = s(node, "path");
    let ty = s(node, "type");
    if !title.is_empty() && !path.is_empty() {
        out.push((title.to_string(), path.to_string(), ty.to_string()));
    }
    if depth < max_depth
        && let Some(children) = node.get("children") {
            flatten_index(children, out, depth + 1, max_depth);
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_path("SwiftUI/View"), "swiftui/view");
        assert_eq!(normalize_path("https://developer.apple.com/documentation/swiftui/view#overview"), "swiftui/view");
        assert_eq!(normalize_path("/documentation/weatherkitrestapi/"), "weatherkitrestapi");
        assert_eq!(normalize_path("doc://com.apple.SwiftUI/documentation/SwiftUI/View/body-8kl5o"), "swiftui/view/body-8kl5o");
    }

    #[test]
    fn renders_basic_doc() {
        let doc = json!({
            "metadata": {"title": "View", "roleHeading": "Protocol", "platforms": [{"name": "iOS", "introducedAt": "13.0"}]},
            "identifier": {"url": "doc://com.apple.SwiftUI/documentation/SwiftUI/View"},
            "abstract": [{"type": "text", "text": "A type that represents UI."}],
            "primaryContentSections": [
                {"kind": "declarations", "declarations": [{"languages": ["swift"], "tokens": [{"text": "protocol View"}]}]},
                {"kind": "content", "content": [
                    {"type": "heading", "level": 2, "text": "Overview"},
                    {"type": "paragraph", "inlineContent": [{"type": "text", "text": "Use "}, {"type": "reference", "identifier": "doc://x/body"}, {"type": "text", "text": "."}]},
                    {"type": "codeListing", "syntax": "swift", "code": ["struct A: View {}"]}
                ]}
            ],
            "topicSections": [{"title": "Essentials", "identifiers": ["doc://x/body"]}],
            "references": {"doc://x/body": {"title": "body", "url": "/documentation/swiftui/view/body", "role": "symbol", "abstract": [{"type": "text", "text": "The content."}]}}
        });
        let md = render_doc(&doc, "all", 10_000);
        assert!(md.starts_with("# View (Protocol)"));
        assert!(md.contains("Availability: iOS 13.0+"));
        assert!(md.contains("```swift\nprotocol View\n```"));
        assert!(md.contains("Use [body](/documentation/swiftui/view/body)."));
        assert!(md.contains("### Essentials\n- body (symbol) — The content. `/documentation/swiftui/view/body`"));
        let ov = render_doc(&doc, "overview", 10_000);
        assert!(!ov.contains("Essentials"));
    }

    #[test]
    fn flattens_index() {
        let idx = json!([{"title": "A", "path": "/a", "type": "module", "children": [{"title": "B", "path": "/a/b", "type": "struct", "children": [{"title": "C", "path": "/a/b/c", "type": "method"}]}]}]);
        let mut rows = Vec::new();
        flatten_index(&idx, &mut rows, 0, 10);
        assert_eq!(rows.len(), 3);
        let mut shallow = Vec::new();
        flatten_index(&idx, &mut shallow, 0, 1);
        assert_eq!(shallow.len(), 2);
    }
}
