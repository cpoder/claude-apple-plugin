//! HTTP helpers: send requests, parse JSON, render bounded output, explain common Apple errors.

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::ToolError;

/// Maximum characters returned by a tool (keeps the model's context manageable).
pub const MAX_OUT: usize = 50_000;

pub struct Resp {
    pub status: u16,
    pub headers: HeaderMap,
    pub bytes: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.bytes).ok()
    }
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    }
}

pub async fn send(req: reqwest::RequestBuilder) -> Result<Resp, ToolError> {
    let resp = req.send().await.map_err(|e| ToolError::new(format!("request failed: {e}")))?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.map_err(|e| ToolError::new(format!("reading body failed: {e}")))?.to_vec();
    Ok(Resp { status, headers, bytes })
}

/// Send and return the parsed JSON body; non-2xx becomes a descriptive error.
pub async fn json(req: reqwest::RequestBuilder, what: &str) -> Result<Value, ToolError> {
    let resp = send(req).await?;
    if !resp.ok() {
        return Err(http_error(what, &resp));
    }
    if resp.bytes.is_empty() {
        return Ok(Value::Object(serde_json::Map::from_iter([(
            "status".to_string(),
            Value::from(resp.status),
        )])));
    }
    match resp.json() {
        Some(v) => Ok(v),
        None => Ok(serde_json::json!({ "status": resp.status, "body": clip(&resp.text(), 4000) })),
    }
}

pub fn http_error(what: &str, resp: &Resp) -> ToolError {
    let body = resp.text();
    let detail = resp
        .json()
        .and_then(|v| {
            // Apple APIs: {"errors":[{"status","code","title","detail"}]} or {"errorMessage"} / {"reason"}
            if let Some(errs) = v.get("errors").and_then(|e| e.as_array()) {
                let msgs: Vec<String> = errs
                    .iter()
                    .map(|e| {
                        let title = e.get("title").and_then(|t| t.as_str()).unwrap_or("");
                        let det = e.get("detail").and_then(|t| t.as_str()).unwrap_or("");
                        let code = e.get("code").and_then(|t| t.as_str()).unwrap_or("");
                        format!("{code} {title}: {det}").trim().to_string()
                    })
                    .collect();
                Some(msgs.join(" | "))
            } else {
                v.get("errorMessage")
                    .or_else(|| v.get("reason"))
                    .or_else(|| v.get("error_description"))
                    .or_else(|| v.get("error"))
                    .or_else(|| v.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            }
        })
        .unwrap_or_else(|| clip(body.trim(), 1500));
    let hint = match resp.status {
        401 => " Hint: token rejected — check key_id/issuer_id/team_id/private key, that the key is enabled for this service, and the system clock.",
        403 => " Hint: the key/user lacks permission for this resource (role, scope, or agreement not accepted in App Store Connect).",
        404 => " Hint: resource or endpoint not found — check IDs and the path.",
        409 => " Hint: conflict (state does not allow this operation).",
        429 => " Hint: rate limited — wait and retry with smaller requests.",
        _ => "",
    };
    ToolError::new(format!("{what}: HTTP {}{}{hint}", resp.status, if detail.is_empty() { String::new() } else { format!(" — {detail}") }))
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Pretty JSON, bounded to MAX_OUT.
pub fn render(v: &Value) -> String {
    bounded(serde_json::to_string_pretty(v).unwrap_or_default())
}

pub fn bounded(s: String) -> String {
    if s.len() <= MAX_OUT {
        return s;
    }
    let mut cut = MAX_OUT;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n[... output truncated at {} of {} chars — narrow the request (limit/fields/filters) ...]",
        &s[..cut],
        MAX_OUT,
        s.len()
    )
}

pub fn method(m: &str) -> Result<reqwest::Method, ToolError> {
    reqwest::Method::from_bytes(m.trim().to_uppercase().as_bytes()).map_err(|_| ToolError::new(format!("invalid HTTP method: {m}")))
}

/// Convert an optional JSON object of query params into (k, v) string pairs (arrays joined with commas).
pub fn query_pairs(q: &Option<serde_json::Map<String, Value>>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(map) = q {
        for (k, v) in map {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().map(|x| match x { Value::String(s) => s.clone(), other => other.to_string() }).collect::<Vec<_>>().join(","),
                Value::Null => continue,
                other => other.to_string(),
            };
            out.push((k.clone(), s));
        }
    }
    out
}

/// Remove `links` objects everywhere (JSON:API noise) and relationships that only carry links.
pub fn strip_links(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("links");
            if let Some(Value::Object(rels)) = map.get_mut("relationships") {
                rels.retain(|_, r| r.get("data").is_some());
                if rels.is_empty() {
                    map.remove("relationships");
                }
            }
            for (_, child) in map.iter_mut() {
                strip_links(child);
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(strip_links),
        _ => {}
    }
}
