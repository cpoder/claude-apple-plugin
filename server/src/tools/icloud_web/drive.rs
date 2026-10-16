//! iCloud Drive through the icloud.com `drivews` / `docws` web services: browse folders by path,
//! download, upload and create folders. Files are addressed by their path under the Drive root
//! (`Documents/report.pdf`).

use std::path::{Path, PathBuf};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{WebErr, WebSession};
use crate::error::{ToolError, ToolResult};
use crate::http::render;
use crate::server::AppleServer;

const ZONE: &str = "com.apple.CloudDocs";
const ROOT_ID: &str = "FOLDER::com.apple.CloudDocs::root";

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// Path segments of a Drive path (`/Documents/./a.txt` → `["Documents", "a.txt"]`).
pub fn split_path(path: Option<&str>) -> Vec<String> {
    path.unwrap_or("").split('/').map(str::trim).filter(|s| !s.is_empty() && *s != ".").map(String::from).collect()
}

/// Display name of a node: `name` plus `.extension`; the root is `root`.
pub fn display_name(item: &Value) -> String {
    let name = item["name"].as_str().unwrap_or("");
    let base = if name.is_empty() {
        if item["drivewsid"] == ROOT_ID { "root".to_string() } else { "<UNKNOWN>".to_string() }
    } else {
        name.to_string()
    };
    match item["extension"].as_str().filter(|e| !e.is_empty()) {
        Some(e) => format!("{base}.{e}"),
        None => base,
    }
}

/// Lower-cased node type (`folder`, `file`, `app_library`, `trash`, `unknown`).
pub fn item_type(item: &Value) -> String {
    match item["type"].as_str() {
        Some(t) => t.to_lowercase(),
        None if item["drivewsid"] == "TRASH_ROOT" || item["docwsid"] == "TRASH_ROOT" => "trash".into(),
        None => "unknown".into(),
    }
}

fn is_container(item: &Value) -> bool {
    matches!(item_type(item).as_str(), "folder" | "app_library" | "trash")
}

fn iso(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    Some(
        chrono::DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_else(|_| s.to_string()),
    )
}

/// Compact summary of a Drive node.
pub fn item_json(item: &Value) -> Value {
    let ty = item_type(item);
    let mut j = json!({
        "name": display_name(item),
        "type": ty,
        "size": item["size"].as_u64().filter(|s| *s > 0),
        "modified": iso(&item["dateModified"]).or_else(|| iso(&item["dateChanged"])),
        "created": iso(&item["dateCreated"]),
        "docwsid": item["docwsid"],
        "drivewsid": item["drivewsid"],
    });
    if is_container(item)
        && let Some(n) = item["directChildrenCount"].as_u64()
    {
        j["children"] = json!(n);
    }
    if !item["shareID"].is_null() || !item["share"].is_null() {
        j["shared"] = json!(true);
    }
    j
}

/// `urllib.parse.quote(s, safe="")`: percent-encode everything but unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The `t=` field of the `X-APPLE-WEBAUTH-VALIDATE` cookie (`v=1:t=<token>:…`).
pub fn validate_token(cookie: &str) -> Option<String> {
    cookie.split(':').find_map(|p| p.strip_prefix("t=")).filter(|t| !t.is_empty()).map(String::from)
}

/// Best-effort MIME type from the file extension (`""` when unknown, like `mimetypes.guess_type`).
pub fn mime_of(name: &str) -> &'static str {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "rtf" => "application/rtf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "m4v" => "video/x-m4v",
        "mov" => "video/quicktime",
        "epub" => "application/epub+zip",
        _ => "",
    }
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(p)
}

/// Write bytes to `destination` (a file path, or a directory in which `default_name` is used),
/// mode 600, refusing to overwrite unless asked.
pub fn write_local(destination: &str, default_name: &str, bytes: &[u8], overwrite: bool) -> Result<PathBuf, ToolError> {
    let destination = destination.trim();
    if destination.is_empty() {
        return Err(ToolError::new("destination path is required"));
    }
    let mut dest = expand_home(destination);
    if dest.is_dir() || destination.ends_with('/') {
        dest = dest.join(default_name);
    }
    if dest.exists() && !overwrite {
        return Err(ToolError::new(format!("{} already exists; pass overwrite=true to replace it", dest.display())));
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::new(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(&dest, bytes).map_err(|e| ToolError::new(format!("cannot write {}: {e}", dest.display())))?;
    crate::setup::restrict(&dest, 0o600);
    Ok(dest)
}

// ---------------------------------------------------------------------------------------------
// Wire calls
// ---------------------------------------------------------------------------------------------

impl WebSession {
    /// POST JSON with `Content-Type: plain/text` (what pyicloud sends to docws/createFolders/batch).
    pub async fn post_plain(&self, url: &str, extra: &[(&str, &str)], body: &Value) -> Result<Value, WebErr> {
        let mut params = self.params();
        params.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let r = self.raw(self.http.post(url).query(&params).header("Content-Type", "plain/text").body(body.to_string())).await?;
        Ok(r.json().unwrap_or(Value::Null))
    }

    /// `retrieveItemDetailsInFolders` for one node (folders come back with their `items`).
    pub async fn drive_node(&self, drivewsid: &str, share_id: Option<&Value>) -> Result<Value, WebErr> {
        let root = self.ws_url("drivews")?;
        let mut item = json!({"drivewsid": drivewsid, "partialData": false});
        if let Some(sid) = share_id.filter(|v| !v.is_null()) {
            item["shareID"] = sid.clone();
        }
        let v = self.post(&format!("{root}/retrieveItemDetailsInFolders"), &[], &json!([item])).await?;
        v.as_array().and_then(|a| a.first().cloned()).ok_or_else(|| WebErr::Other(format!("iCloud Drive: unexpected answer for {drivewsid}")))
    }

    /// Walk from the Drive root along `segments`; a folder comes with its `items`, a file as its
    /// parent-listing entry.
    pub async fn drive_resolve(&self, segments: &[String]) -> Result<Value, WebErr> {
        self.ensure_pcs("iclouddrive").await?;
        let mut node = self.drive_node(ROOT_ID, None).await?;
        for (i, seg) in segments.iter().enumerate() {
            let so_far = segments[..i].join("/");
            let items = node["items"]
                .as_array()
                .ok_or_else(|| WebErr::Other(format!("iCloud Drive: no items in folder `/{so_far}` (status {})", node["status"].as_str().unwrap_or("?"))))?;
            let child = items
                .iter()
                .find(|c| display_name(c) == *seg)
                .or_else(|| items.iter().find(|c| c["name"].as_str() == Some(seg.as_str())))
                .cloned()
                .ok_or_else(|| WebErr::Other(format!("iCloud Drive: no item named `{seg}` in `/{so_far}`")))?;
            if is_container(&child) {
                let id = child["drivewsid"].as_str().ok_or_else(|| WebErr::Other(format!("iCloud Drive: `{seg}` has no drivewsid")))?;
                node = self.drive_node(id, child.get("shareID")).await?;
                if node["shareID"].is_null()
                    && let Some(sid) = child.get("shareID").filter(|v| !v.is_null())
                {
                    node["shareID"] = sid.clone();
                }
            } else if i + 1 < segments.len() {
                return Err(WebErr::Other(format!("iCloud Drive: `/{so_far}/{seg}` is a file, not a folder")));
            } else {
                node = child;
            }
        }
        Ok(node)
    }

    /// Bytes of a file node (two steps: signed CDN URL from docws, then the content).
    pub async fn drive_download(&self, file: &Value) -> Result<Vec<u8>, WebErr> {
        if file["size"].as_u64().unwrap_or(0) == 0 {
            return Ok(Vec::new());
        }
        let docws = self.ws_url("docws")?;
        let zone = file["zone"].as_str().unwrap_or(ZONE);
        let docwsid = file["docwsid"].as_str().ok_or_else(|| WebErr::Other("iCloud Drive: file without docwsid".into()))?;
        let prefix = file["drivewsid"].as_str().unwrap_or("").split("::").next().unwrap_or("");
        let owner = if matches!(prefix, "FILE_IN_SHARED_FOLDER" | "FOLDER_IN_SHARED_FOLDER") && (!file["share"].is_null() || !file["shareID"].is_null()) {
            file["owner"]["ownerRecordName"].as_str().filter(|s| !s.is_empty())
        } else {
            None
        };
        let url = match owner {
            Some(o) => format!("{docws}/ws/{zone}/download/by_id/{}", percent_encode(o)),
            None => format!("{docws}/ws/{zone}/download/by_id"),
        };
        let tok = self.get(&url, &[("document_id", docwsid)]).await?;
        let cdn = tok["data_token"]["url"]
            .as_str()
            .or_else(|| tok["package_token"]["url"].as_str())
            .ok_or_else(|| WebErr::Other("iCloud Drive: neither data_token nor package_token in the download answer".into()))?;
        let r = self.raw(self.http.get(cdn).query(&self.params())).await?;
        Ok(r.body)
    }

    /// Upload `data` as `name` into a folder node (reserve → multipart upload → register).
    pub async fn drive_upload(&self, folder: &Value, name: &str, data: Vec<u8>) -> Result<Value, WebErr> {
        let token = self
            .cookie("X-APPLE-WEBAUTH-VALIDATE")
            .and_then(|c| validate_token(&c))
            .ok_or_else(|| WebErr::Other("iCloud Drive upload: X-APPLE-WEBAUTH-VALIDATE cookie not found; run icloud_login again".into()))?;
        let docws = self.ws_url("docws")?;
        let zone = folder["zone"].as_str().unwrap_or(ZONE).to_string();
        let folder_id = folder["docwsid"].as_str().unwrap_or("root").to_string();
        let ct = mime_of(name);
        let extra = [("token", token.as_str())];
        let size = data.len();
        let reserve = self
            .post_plain(&format!("{docws}/ws/{zone}/upload/web"), &extra, &json!({"filename": name, "type": "FILE", "content_type": ct, "size": size}))
            .await?;
        let slot = reserve.as_array().and_then(|a| a.first()).ok_or_else(|| WebErr::Other(format!("iCloud Drive upload: unexpected reservation answer: {}", crate::http::clip(&reserve.to_string(), 300))))?;
        let doc_id = slot["document_id"].as_str().ok_or_else(|| WebErr::Other("iCloud Drive upload: no document_id".into()))?.to_string();
        let upload_url = slot["url"].as_str().ok_or_else(|| WebErr::Other("iCloud Drive upload: no upload url".into()))?.to_string();
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(name.to_string())
            .mime_str(if ct.is_empty() { "application/octet-stream" } else { ct })
            .map_err(|e| WebErr::Other(format!("multipart: {e}")))?;
        let form = reqwest::multipart::Form::new().part(name.to_string(), part);
        let up = self.raw(self.http.post(&upload_url).multipart(form)).await?.json().unwrap_or(Value::Null);
        let sf = &up["singleFile"];
        if !sf.is_object() {
            return Err(WebErr::Other(format!("iCloud Drive upload: content server did not return singleFile: {}", crate::http::clip(&up.to_string(), 300))));
        }
        let mut data_obj = json!({"signature": sf["fileChecksum"], "wrapping_key": sf["wrappingKey"], "reference_signature": sf["referenceChecksum"], "size": sf["size"]});
        if !sf["receipt"].is_null() {
            data_obj["receipt"] = sf["receipt"].clone();
        }
        let now = chrono::Utc::now().timestamp_millis();
        let body = json!({
            "data": data_obj,
            "command": "add_file",
            "create_short_guid": true,
            "document_id": doc_id,
            "path": {"starting_document_id": folder_id, "path": name},
            "allow_conflict": true,
            "file_flags": {"is_writable": true, "is_executable": false, "is_hidden": false},
            "mtime": now,
            "btime": now,
        });
        self.post_plain(&format!("{docws}/ws/{zone}/update/documents"), &extra, &body).await
    }

    /// `createFolders` under a parent drivewsid.
    pub async fn drive_mkdir(&self, parent_drivewsid: &str, name: &str) -> Result<Value, WebErr> {
        let root = self.ws_url("drivews")?;
        let body = json!({
            "destinationDrivewsId": parent_drivewsid,
            "folders": [{"clientId": format!("FOLDER::UNKNOWN_ZONE::TempId-{}", uuid::Uuid::new_v4()), "name": name}],
        });
        self.post_plain(&format!("{root}/createFolders"), &[], &body).await
    }
}

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveListParams {
    /// Folder path under the Drive root, e.g. `Documents/Work` (default: the root)
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveDownloadParams {
    /// Path of the file in iCloud Drive, e.g. `Documents/report.pdf`
    pub path: String,
    /// Local file path (or existing directory) to write to
    pub destination: String,
    /// Replace an existing local file (default false)
    pub overwrite: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveUploadParams {
    /// Local file to upload
    pub local_path: String,
    /// Destination folder path in iCloud Drive (default: the root)
    pub folder: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveMkdirParams {
    /// Parent folder path (default: the root)
    pub parent: Option<String>,
    /// Name of the new folder
    pub name: String,
}

fn require_folder(node: &Value, path: &str) -> Result<(), ToolError> {
    if is_container(node) { Ok(()) } else { Err(ToolError::new(format!("iCloud Drive: `{path}` is not a folder"))) }
}

#[tool_router(router = icloud_drive_router, vis = "pub")]
impl AppleServer {
    /// List an iCloud Drive folder (name, type, size, modified date) by path; a file path returns that file's entry.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_drive_list(&self, Parameters(p): Parameters<DriveListParams>) -> ToolResult {
        let s = self.web().await?;
        let segments = split_path(p.path.as_deref());
        let node = s.drive_resolve(&segments).await?;
        let path = format!("/{}", segments.join("/"));
        if !is_container(&node) {
            return Ok(render(&json!({"path": path, "item": item_json(&node)})));
        }
        let items: Vec<Value> = node["items"].as_array().map(|a| a.iter().map(item_json).collect()).unwrap_or_default();
        Ok(render(&json!({"path": path, "count": items.len(), "items": items})))
    }

    /// Download a file from iCloud Drive to a local path (written with mode 600; existing files are kept unless overwrite=true).
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_drive_download(&self, Parameters(p): Parameters<DriveDownloadParams>) -> ToolResult {
        let s = self.web().await?;
        let segments = split_path(Some(&p.path));
        if segments.is_empty() {
            return Err(ToolError::new("path of the file to download is required"));
        }
        let node = s.drive_resolve(&segments).await?;
        if is_container(&node) {
            return Err(ToolError::new(format!("iCloud Drive: `{}` is a folder; give a file path", p.path)));
        }
        let name = display_name(&node);
        let bytes = s.drive_download(&node).await?;
        let dest = write_local(&p.destination, &name, &bytes, p.overwrite.unwrap_or(false))?;
        Ok(render(&json!({"path": dest.display().to_string(), "bytes": bytes.len(), "name": name, "modified": iso(&node["dateModified"])})))
    }

    /// Upload a local file into an iCloud Drive folder (the file keeps its base name).
    #[tool]
    pub async fn icloud_drive_upload(&self, Parameters(p): Parameters<DriveUploadParams>) -> ToolResult {
        let s = self.web().await?;
        let local = expand_home(p.local_path.trim());
        let name = local.file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).ok_or_else(|| ToolError::new("local_path must point to a file"))?.to_string();
        let data = std::fs::read(&local).map_err(|e| ToolError::new(format!("cannot read {}: {e}", local.display())))?;
        let segments = split_path(p.folder.as_deref());
        let folder = s.drive_resolve(&segments).await?;
        let folder_path = format!("/{}", segments.join("/"));
        require_folder(&folder, &folder_path)?;
        let size = data.len();
        let resp = s.drive_upload(&folder, &name, data).await?;
        Ok(render(&json!({
            "uploaded": name,
            "folder": folder_path,
            "bytes": size,
            "status": resp["status"],
            "document_id": resp["document_id"],
            "etag": resp["etag"],
        })))
    }

    /// Create a folder in iCloud Drive under a parent folder path (default: the root).
    #[tool]
    pub async fn icloud_drive_mkdir(&self, Parameters(p): Parameters<DriveMkdirParams>) -> ToolResult {
        let s = self.web().await?;
        let name = p.name.trim();
        if name.is_empty() || name.contains('/') {
            return Err(ToolError::new("folder name must be non-empty and contain no `/`"));
        }
        let segments = split_path(p.parent.as_deref());
        let parent = s.drive_resolve(&segments).await?;
        let parent_path = format!("/{}", segments.join("/"));
        require_folder(&parent, &parent_path)?;
        let parent_id = parent["drivewsid"].as_str().unwrap_or(ROOT_ID);
        let resp = s.drive_mkdir(parent_id, name).await?;
        let created = resp["folders"].as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
        if created["status"].as_str().is_some_and(|st| st != "OK") {
            return Err(ToolError::new(format!("iCloud Drive: createFolders answered {}", created["status"])));
        }
        Ok(render(&json!({"created": item_json(&created), "parent": parent_path})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_splitting() {
        assert!(split_path(None).is_empty());
        assert!(split_path(Some("/")).is_empty());
        assert_eq!(split_path(Some("/Documents//Work/./a b.txt/")), vec!["Documents", "Work", "a b.txt"]);
        assert_eq!(split_path(Some(" Documents ")), vec!["Documents"]);
    }

    #[test]
    fn names_and_types() {
        assert_eq!(display_name(&json!({"name": "report", "extension": "pdf", "type": "FILE"})), "report.pdf");
        assert_eq!(display_name(&json!({"name": "Documents", "type": "FOLDER"})), "Documents");
        assert_eq!(display_name(&json!({"name": "", "drivewsid": ROOT_ID})), "root");
        assert_eq!(display_name(&json!({"drivewsid": "FOLDER::x::y"})), "<UNKNOWN>");
        assert_eq!(item_type(&json!({"type": "APP_LIBRARY"})), "app_library");
        assert_eq!(item_type(&json!({"drivewsid": "TRASH_ROOT"})), "trash");
        assert_eq!(item_type(&json!({})), "unknown");
        let j = item_json(&json!({"name": "a", "extension": "txt", "type": "FILE", "size": 0, "dateModified": "2024-05-06T07:08:09+02:00", "docwsid": "D"}));
        assert!(j["size"].is_null());
        assert_eq!(j["modified"], "2024-05-06T05:08:09Z");
        assert_eq!(j["name"], "a.txt");
    }

    #[test]
    fn token_and_encoding() {
        assert_eq!(validate_token("v=1:t=AbC-123:x=y").as_deref(), Some("AbC-123"));
        assert_eq!(validate_token("t=solo").as_deref(), Some("solo"));
        assert!(validate_token("v=1:x=y").is_none());
        assert!(validate_token("v=1:t=:x").is_none());
        assert_eq!(percent_encode("_abc 1/2=3~"), "_abc%201%2F2%3D3~");
        assert_eq!(mime_of("Photo.JPG"), "image/jpeg");
        assert_eq!(mime_of("/tmp/x.pdf"), "application/pdf");
        assert_eq!(mime_of("weird.bin"), "");
        assert_eq!(mime_of("noext"), "");
    }

    #[test]
    fn local_write_refuses_overwrite() {
        let dir = std::env::temp_dir().join(format!("apple-mcp-drive-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = write_local(&dir.display().to_string(), "f.txt", b"hi", false).unwrap();
        assert_eq!(p, dir.join("f.txt"));
        assert!(write_local(&p.display().to_string(), "f.txt", b"again", false).is_err());
        assert!(write_local(&p.display().to_string(), "f.txt", b"again", true).is_ok());
        assert_eq!(std::fs::read(&p).unwrap(), b"again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
