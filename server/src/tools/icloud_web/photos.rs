//! iCloud Photos through CloudKit web services (container `com.apple.photos.cloud`, zone
//! `PrimarySync`), as the icloud.com Photos app does it: smart albums, user albums (with
//! folders), rank-paged asset listing newest first, and downloads of the original / medium /
//! thumbnail renditions.

use std::collections::{HashMap, HashSet, VecDeque};

use base64::prelude::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::drive::write_local;
use super::{WebErr, WebSession};
use crate::error::{ToolError, ToolResult};
use crate::http::render;
use crate::server::AppleServer;

const PRIVATE: &str = "/database/1/com.apple.photos.cloud/production/private";
/// Apple's "unset" timestamp sentinels (0001-01-01).
const NULL_TS: i64 = -62135596800000;
const NULL_TS2: i64 = -62135769600000;
/// pyicloud page size: a page is `2 * PAGE_SIZE` records (master + asset pairs).
const PAGE_SIZE: usize = 100;
/// Safety cap on the number of pages scanned in one call.
const MAX_PAGES: usize = 2000;
const CK_PARAMS: &[(&str, &str)] = &[("remapEnums", "True"), ("getCurrentSyncToken", "True")];

fn zone() -> Value {
    json!({"zoneName": "PrimarySync", "zoneType": "REGULAR_CUSTOM_ZONE"})
}

// ---------------------------------------------------------------------------------------------
// Albums
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub fullname: String,
    /// `smart`, `album` or `folder`.
    pub kind: &'static str,
    /// Count container id prefix (`HyperionIndexCountLookup`).
    pub obj_type: String,
    /// Query record type for listing.
    pub list_type: String,
    pub descending: bool,
    pub filters: Vec<Value>,
}

impl Album {
    /// `indexCountID` for the batch count query.
    pub fn count_id(&self) -> String {
        if self.kind == "smart" { self.obj_type.clone() } else { format!("{}:{}", self.obj_type, self.id) }
    }
}

/// (name, obj_type, list_type, descending, smartAlbum filter)
const SMART_ALBUMS: &[(&str, &str, &str, bool, Option<&str>)] = &[
    ("Library", "CPLAssetByAssetDateWithoutHiddenOrDeleted", "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted", true, None),
    ("Time-lapse", "CPLAssetInSmartAlbumByAssetDate:Timelapse", "CPLAssetAndMasterInSmartAlbumByAssetDate", false, Some("TIMELAPSE")),
    ("Videos", "CPLAssetInSmartAlbumByAssetDate:Video", "CPLAssetAndMasterInSmartAlbumByAssetDate", false, Some("VIDEO")),
    ("Slo-mo", "CPLAssetInSmartAlbumByAssetDate:Slomo", "CPLAssetAndMasterInSmartAlbumByAssetDate", false, Some("SLOMO")),
    ("Bursts", "CPLAssetBurstStackAssetByAssetDate", "CPLBurstStackAssetAndMasterByAssetDate", false, None),
    ("Favorites", "CPLAssetInSmartAlbumByAssetDate:Favorite", "CPLAssetAndMasterInSmartAlbumByAssetDate", false, Some("FAVORITE")),
    ("Panoramas", "CPLAssetInSmartAlbumByAssetDate:Panorama", "CPLAssetAndMasterInSmartAlbumByAssetDate", false, Some("PANORAMA")),
    ("Screenshots", "CPLAssetInSmartAlbumByAssetDate:Screenshot", "CPLAssetAndMasterInSmartAlbumByAssetDate", false, Some("SCREENSHOT")),
    ("Live", "CPLAssetInSmartAlbumByAssetDate:Live", "CPLAssetAndMasterInSmartAlbumByAssetDate", false, Some("LIVE")),
    ("Recently Deleted", "CPLAssetDeletedByExpungedDate", "CPLAssetAndMasterDeletedByExpungedDate", false, None),
    ("Hidden", "CPLAssetHiddenByAssetDate", "CPLAssetAndMasterHiddenByAssetDate", false, None),
];

pub fn smart_albums() -> Vec<Album> {
    SMART_ALBUMS
        .iter()
        .map(|(name, obj, list, desc, filter)| Album {
            id: (*name).to_string(),
            name: (*name).to_string(),
            fullname: (*name).to_string(),
            kind: "smart",
            obj_type: (*obj).to_string(),
            list_type: (*list).to_string(),
            descending: *desc,
            filters: filter.map(|f| vec![json!({"comparator": "EQUALS", "fieldName": "smartAlbum", "fieldValue": {"type": "STRING", "value": f}})]).unwrap_or_default(),
        })
        .collect()
}

/// Decode an `…Enc` field (base64 of UTF-8); falls back to the raw value when it is not base64.
pub fn decode_enc(field: &Value) -> Option<String> {
    let raw = field["value"].as_str()?;
    Some(BASE64_STANDARD.decode(raw.trim()).ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_else(|| raw.to_string()))
}

fn int(v: &Value) -> Option<i64> {
    v["value"].as_i64().or_else(|| v["value"].as_str().and_then(|s| s.parse().ok()))
}

fn ts(v: &Value) -> Option<String> {
    let ms = v["value"].as_i64()?;
    if ms <= NULL_TS || ms == NULL_TS2 {
        return None;
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

// ---------------------------------------------------------------------------------------------
// Assets
// ---------------------------------------------------------------------------------------------

const ITEM_TYPES: &[(&str, &str)] = &[
    ("public.heic", "image"),
    ("public.heif", "image"),
    ("public.jpeg", "image"),
    ("public.png", "image"),
    ("com.apple.quicktime-movie", "movie"),
    ("public.mpeg-4", "movie"),
    ("com.apple.m4v-video", "movie"),
    ("com.adobe.raw-image", "image"),
    ("com.canon.cr2-raw-image", "image"),
    ("com.canon.cr3-raw-image", "image"),
    ("com.canon.crw-raw-image", "image"),
    ("com.fuji.raw-image", "image"),
    ("com.nikon.nrw-raw-image", "image"),
    ("com.nikon.raw-image", "image"),
    ("com.olympus.or-raw-image", "image"),
    ("com.olympus.raw-image", "image"),
    ("com.panasonic.rw2-raw-image", "image"),
    ("com.pentax.raw-image", "image"),
    ("com.sony.arw-raw-image", "image"),
];

const FILE_TYPE_EXTENSIONS: &[(&str, &str)] = &[
    ("public.heic", ".HEIC"),
    ("public.heif", ".HEIF"),
    ("public.jpeg", ".JPG"),
    ("public.png", ".PNG"),
    ("com.apple.quicktime-movie", ".MOV"),
    ("public.mpeg-4", ".MP4"),
    ("com.apple.m4v-video", ".M4V"),
    ("com.adobe.raw-image", ".DNG"),
    ("com.canon.cr2-raw-image", ".CR2"),
    ("com.canon.cr3-raw-image", ".CR3"),
    ("com.canon.crw-raw-image", ".CRW"),
    ("com.fuji.raw-image", ".RAF"),
    ("com.nikon.nrw-raw-image", ".NRF"),
    ("com.nikon.raw-image", ".NEF"),
    ("com.olympus.or-raw-image", ".ORF"),
    ("com.olympus.raw-image", ".ORF"),
    ("com.panasonic.rw2-raw-image", ".RW2"),
    ("com.pentax.raw-image", ".PEF"),
    ("com.sony.arw-raw-image", ".ARW"),
];

const PHOTO_VERSIONS: &[(&str, &str)] = &[
    ("original", "resOriginal"),
    ("alternative", "resOriginalAlt"),
    ("medium", "resJPEGMed"),
    ("thumb", "resJPEGThumb"),
    ("original_video", "resOriginalVidCompl"),
    ("medium_video", "resVidMed"),
    ("thumb_video", "resVidSmall"),
    ("sidecar", "resSidecar"),
];

const VIDEO_VERSIONS: &[(&str, &str)] = &[("original", "resOriginal"), ("medium", "resVidMed"), ("thumb", "resVidSmall"), ("thumb_image", "resJPEGThumb"), ("medium_image", "resJPEGMed")];

const RAW_EXTENSIONS: &[&str] = &[".arw", ".cr2", ".cr3", ".crw", ".dng", ".nef", ".nrf", ".nrw", ".orf", ".pef", ".raf", ".rw2"];

fn uti_kind(uti: &str) -> Option<&'static str> {
    ITEM_TYPES.iter().find(|(u, _)| *u == uti).map(|(_, k)| *k)
}

/// Filename of a master record (decoded `filenameEnc`, else the asset id).
pub fn filename_of(master: &Value, asset_id: &str) -> String {
    decode_enc(&master["fields"]["filenameEnc"]).filter(|s| !s.is_empty()).unwrap_or_else(|| asset_id.to_string())
}

/// `image` or `movie`, from the master's UTIs then the filename.
pub fn item_type(master: &Value, filename: &str) -> &'static str {
    let f = &master["fields"];
    if let Some(k) = f["itemType"]["value"].as_str().and_then(uti_kind) {
        return k;
    }
    let orig = f["resOriginalFileType"]["value"].as_str().unwrap_or("");
    if let Some(k) = uti_kind(orig) {
        return k;
    }
    if orig.contains("raw") {
        return "image";
    }
    let lower = filename.to_lowercase();
    if [".heic", ".png", ".jpg", ".jpeg"].iter().chain(RAW_EXTENSIONS).any(|e| lower.ends_with(e)) { "image" } else { "movie" }
}

/// `CPLMaster` field prefix of a rendition (`original` / `medium` / `thumb` …) for an item kind.
pub fn version_prefix(kind: &str, version: &str) -> Option<&'static str> {
    let table = if kind == "movie" { VIDEO_VERSIONS } else { PHOTO_VERSIONS };
    table.iter().find(|(k, _)| *k == version).map(|(_, p)| *p)
}

/// Local filename of a rendition: the master's stem with the rendition's extension when known.
pub fn resource_filename(filename: &str, file_type: &str) -> String {
    match FILE_TYPE_EXTENSIONS.iter().find(|(u, _)| *u == file_type) {
        Some((_, ext)) => {
            let stem = match filename.rfind('.') {
                Some(i) if i > 0 => &filename[..i],
                _ => filename,
            };
            format!("{stem}{ext}")
        }
        None => filename.to_string(),
    }
}

/// Renditions present on a master (`original`, `medium`, `thumb` …).
fn versions_of(master: &Value, kind: &str) -> Vec<&'static str> {
    let table = if kind == "movie" { VIDEO_VERSIONS } else { PHOTO_VERSIONS };
    table.iter().filter(|(_, p)| master["fields"][format!("{p}Res")].is_object()).map(|(k, _)| *k).collect()
}

/// Compact summary of a (master, asset) pair.
pub fn photo_json(master: &Value, asset: &Value) -> Value {
    let id = asset["recordName"].as_str().unwrap_or("");
    let filename = filename_of(master, id);
    let kind = item_type(master, &filename);
    let mf = &master["fields"];
    let af = &asset["fields"];
    json!({
        "id": id,
        "filename": filename,
        "created": ts(&af["assetDate"]).or_else(|| ts(&af["addedDate"])),
        "added": ts(&af["addedDate"]),
        "size": mf["resOriginalRes"]["value"]["size"],
        "width": mf["resOriginalWidth"]["value"],
        "height": mf["resOriginalHeight"]["value"],
        "type": kind,
        "live_photo": kind == "image" && !mf["resOriginalVidComplFileType"].is_null(),
        "favorite": int(&af["isFavorite"]).unwrap_or(0) != 0,
        "hidden": int(&af["isHidden"]).unwrap_or(0) != 0,
        "duration": af["duration"]["value"].as_f64().filter(|d| *d > 0.0),
        "versions": versions_of(master, kind),
        "master_id": master["recordName"],
    })
}

/// Group a page's records into (master, asset) pairs in master order; `CPLAsset`s join their
/// master through `masterRef` (own id as fallback).
pub fn group_pairs(records: &[Value]) -> Vec<(Value, Value)> {
    let mut by_master: HashMap<String, Vec<&Value>> = HashMap::new();
    for r in records.iter().filter(|r| r["recordType"] == "CPLAsset") {
        let key = r["fields"]["masterRef"]["value"]["recordName"].as_str().or_else(|| r["recordName"].as_str()).unwrap_or("").to_string();
        by_master.entry(key).or_default().push(r);
    }
    let mut out = Vec::new();
    for m in records.iter().filter(|r| r["recordType"] == "CPLMaster") {
        if let Some(assets) = m["recordName"].as_str().and_then(|n| by_master.get(n)) {
            for a in assets {
                out.push((m.clone(), (*a).clone()));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// CloudKit calls
// ---------------------------------------------------------------------------------------------

fn check(v: &Value, what: &str) -> Result<(), WebErr> {
    if let Some(code) = v["serverErrorCode"].as_str() {
        return Err(WebErr::Other(format!("Photos {what}: {code} {}", v["reason"].as_str().unwrap_or(""))));
    }
    Ok(())
}

impl WebSession {
    async fn ck(&self, path: &str, body: &Value) -> Result<Value, WebErr> {
        self.ensure_pcs("photos").await?;
        let base = self.ws_url("ckdatabasews")?;
        let v = self.post(&format!("{base}{PRIVATE}/{path}"), CK_PARAMS, body).await?;
        check(&v, path)?;
        Ok(v)
    }

    /// `CheckIndexingState` must report `FINISHED` before the library can be queried.
    pub async fn photos_ready(&self) -> Result<(), WebErr> {
        let v = self.ck("records/query", &json!({"query": {"recordType": "CheckIndexingState"}, "zoneID": zone(), "resultsLimit": 1})).await?;
        let state = v["records"][0]["fields"]["state"]["value"].as_str().unwrap_or("");
        if state == "FINISHED" {
            Ok(())
        } else {
            Err(WebErr::Other(format!("iCloud Photo Library not finished indexing (state `{state}`); try again in a few minutes")))
        }
    }

    async fn album_records(&self, parent: Option<&str>) -> Result<Vec<Value>, WebErr> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut query = json!({"recordType": "CPLAlbumByPositionLive"});
            if let Some(p) = parent {
                query["filterBy"] = json!([{"comparator": "EQUALS", "fieldName": "parentId", "fieldValue": {"type": "STRING", "value": p}}]);
            }
            let mut body = json!({"query": query, "zoneID": zone()});
            if let Some(m) = &marker {
                body["continuationMarker"] = json!(m);
            }
            let v = self.ck("records/query", &body).await?;
            out.extend(v["records"].as_array().into_iter().flatten().cloned());
            marker = v["continuationMarker"].as_str().filter(|s| !s.is_empty()).map(String::from);
            if marker.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// User albums and folders (folders are walked recursively; `fullname` carries the path).
    pub async fn user_albums(&self) -> Result<Vec<Album>, WebErr> {
        let mut albums: Vec<Album> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(Option<String>, String, Vec<String>)> = VecDeque::from([(None, String::new(), Vec::new())]);
        while let Some((parent, prefix, ancestors)) = queue.pop_front() {
            for r in self.album_records(parent.as_deref()).await? {
                let f = &r["fields"];
                if r["recordType"] != "CPLAlbum" || int(&f["isDeleted"]).unwrap_or(0) != 0 {
                    continue;
                }
                let Some(name) = decode_enc(&f["albumNameEnc"]) else { continue };
                let Some(id) = r["recordName"].as_str().map(String::from) else { continue };
                if !seen.insert(id.clone()) {
                    continue;
                }
                let ty = int(&f["albumType"]).unwrap_or(0);
                let fullname = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                let descending = !f["sortAscending"]["value"].is_null() && int(&f["sortAscending"]) != Some(1);
                albums.push(Album {
                    id: id.clone(),
                    name,
                    fullname: fullname.clone(),
                    kind: if ty == 3 { "folder" } else { "album" },
                    obj_type: "CPLContainerRelationNotDeletedByAssetDate".into(),
                    list_type: "CPLContainerRelationLiveByAssetDate".into(),
                    descending,
                    filters: vec![json!({"comparator": "EQUALS", "fieldName": "parentId", "fieldValue": {"type": "STRING", "value": id}})],
                });
                if ty == 3 && !ancestors.contains(&id) {
                    let mut anc = ancestors.clone();
                    anc.push(id.clone());
                    queue.push_back((Some(id), fullname, anc));
                }
            }
        }
        Ok(albums)
    }

    /// Resolve an album by name (case-insensitive), full path or id; `All Photos` = `Library`.
    pub async fn resolve_album(&self, wanted: Option<&str>) -> Result<Album, WebErr> {
        let w = wanted.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Library");
        let lw = if w.eq_ignore_ascii_case("All Photos") { "library".to_string() } else { w.to_lowercase() };
        if let Some(a) = smart_albums().into_iter().find(|a| a.name.to_lowercase() == lw) {
            return Ok(a);
        }
        let user = self.user_albums().await?;
        user.iter().find(|a| a.id == w || a.name.to_lowercase() == lw || a.fullname.to_lowercase() == lw).cloned().ok_or_else(|| {
            let names: Vec<&str> = user.iter().map(|a| a.fullname.as_str()).collect();
            WebErr::Other(format!("no Photos album `{w}`; smart albums: Library, Favorites, Videos, Screenshots, Live, Bursts, Panoramas, Slo-mo, Time-lapse, Hidden, Recently Deleted; albums: {}", names.join(", ")))
        })
    }

    async fn count_batch(&self, ids: &[String]) -> Result<Vec<Option<i64>>, WebErr> {
        self.ensure_pcs("photos").await?;
        let base = self.ws_url("ckdatabasews")?;
        let batch: Vec<Value> = ids
            .iter()
            .map(|id| {
                json!({
                    "resultsLimit": 1,
                    "query": {
                        "recordType": "HyperionIndexCountLookup",
                        "filterBy": {"fieldName": "indexCountID", "comparator": "IN", "fieldValue": {"type": "STRING_LIST", "value": [id]}}
                    },
                    "zoneWide": true,
                    "zoneID": zone(),
                })
            })
            .collect();
        let v = self.post_plain(&format!("{base}{PRIVATE}/internal/records/query/batch"), CK_PARAMS, &json!({"batch": batch})).await?;
        check(&v, "count")?;
        match v["batch"].as_array() {
            Some(a) if a.len() == ids.len() => Ok(a.iter().map(|b| b["records"][0]["fields"]["itemCount"]["value"].as_i64()).collect()),
            _ => Err(WebErr::Other("Photos count query failed".into())),
        }
    }

    /// Item counts of albums (`HyperionIndexCountLookup`), batched; `None` when Apple has no index.
    pub async fn album_counts(&self, albums: &[Album]) -> Result<Vec<Option<i64>>, WebErr> {
        let ids: Vec<String> = albums.iter().map(Album::count_id).collect();
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(20) {
            match self.count_batch(chunk).await {
                Ok(c) => out.extend(c),
                Err(_) if chunk.len() > 1 => {
                    for id in chunk {
                        out.push(self.count_batch(std::slice::from_ref(id)).await.ok().and_then(|c| c.into_iter().next().flatten()));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    pub async fn album_len(&self, album: &Album) -> Result<i64, WebErr> {
        self.count_batch(&[album.count_id()]).await?.into_iter().next().flatten().ok_or_else(|| WebErr::Other("Photos count query failed".into()))
    }

    /// One page of (master, asset) pairs from `start_rank`; returns the pairs and the raw count.
    async fn photos_page(&self, album: &Album, start_rank: i64) -> Result<(Vec<(Value, Value)>, usize), WebErr> {
        let mut filters = vec![
            json!({"comparator": "EQUALS", "fieldName": "direction", "fieldValue": {"type": "STRING", "value": if album.descending { "DESCENDING" } else { "ASCENDING" }}}),
            json!({"comparator": "EQUALS", "fieldName": "startRank", "fieldValue": {"type": "INT64", "value": start_rank}}),
        ];
        filters.extend(album.filters.iter().cloned());
        let body = json!({"query": {"recordType": album.list_type, "filterBy": filters}, "zoneID": zone(), "resultsLimit": PAGE_SIZE * 2});
        let v = self.ck("records/query", &body).await?;
        let pairs = group_pairs(v["records"].as_array().map(Vec::as_slice).unwrap_or(&[]));
        let n = pairs.len();
        Ok((pairs, n))
    }

    /// Walk an album from `offset` (newest first for descending albums), handing each unseen
    /// (master, asset) pair to `visit` until it returns `false` or the pages run out.
    pub async fn photos_scan<F: FnMut(Value, Value) -> bool>(&self, album: &Album, offset: usize, mut visit: F) -> Result<(), WebErr> {
        let mut rank: i64 = if album.descending {
            let len = self.album_len(album).await?;
            len - 1 - offset as i64
        } else {
            offset as i64
        };
        if rank < 0 {
            return Ok(());
        }
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..MAX_PAGES {
            let (pairs, n) = self.photos_page(album, rank).await?;
            for (m, a) in pairs {
                if !seen.insert(a["recordName"].as_str().unwrap_or("").to_string()) {
                    continue;
                }
                if !visit(m, a) {
                    return Ok(());
                }
            }
            if n < PAGE_SIZE / 2 {
                break;
            }
            rank = if album.descending { rank - n as i64 } else { rank + n as i64 };
            if rank < 0 {
                break;
            }
        }
        Ok(())
    }

    /// Bytes of a rendition (signed `cvws.icloud-content.com` URL, fetched as-is).
    pub async fn photos_download(&self, url: &str) -> Result<Vec<u8>, WebErr> {
        Ok(self.raw(self.http.get(url)).await?.body)
    }
}

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PhotosListParams {
    /// Album name (`Library` default; `Favorites`, `Videos`, `Screenshots`, …, or a user album / `Folder/Album`)
    pub album: Option<String>,
    /// Max items (default 50, max 500)
    pub limit: Option<usize>,
    /// Items to skip (default 0)
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PhotosDownloadParams {
    /// Filename as shown by icloud_photos_list (e.g. `IMG_1234.HEIC`)
    pub filename: String,
    /// Local file path (or existing directory) to write to
    pub destination: String,
    /// Album to search (default `Library`)
    pub album: Option<String>,
    /// `original` (default), `medium` or `thumb`
    pub version: Option<String>,
    /// Replace an existing local file (default false)
    pub overwrite: Option<bool>,
}

#[tool_router(router = icloud_photos_router, vis = "pub")]
impl AppleServer {
    /// List iCloud Photos albums (smart albums such as Library, Favorites, Videos, plus user albums and folders) with item counts.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_photos_albums(&self) -> ToolResult {
        let s = self.web().await?;
        s.photos_ready().await?;
        let mut albums = smart_albums();
        albums.extend(s.user_albums().await?);
        let counts = s.album_counts(&albums).await?;
        let out: Vec<Value> = albums
            .iter()
            .zip(counts)
            .map(|(a, c)| json!({"id": a.id, "name": a.name, "path": a.fullname, "kind": a.kind, "count": c}))
            .collect();
        Ok(render(&json!({"count": out.len(), "albums": out})))
    }

    /// List the photos and videos of an album newest first (filename, date, size, dimensions, type) with limit/offset paging.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_photos_list(&self, Parameters(p): Parameters<PhotosListParams>) -> ToolResult {
        let s = self.web().await?;
        s.photos_ready().await?;
        let album = s.resolve_album(p.album.as_deref()).await?;
        let limit = p.limit.unwrap_or(50).clamp(1, 500);
        let offset = p.offset.unwrap_or(0);
        let mut items: Vec<Value> = Vec::new();
        s.photos_scan(&album, offset, |m, a| {
            items.push(photo_json(&m, &a));
            items.len() < limit
        })
        .await?;
        Ok(render(&json!({"album": {"id": album.id, "name": album.name, "path": album.fullname}, "offset": offset, "count": items.len(), "items": items})))
    }

    /// Download a photo or video by filename from an album to a local path, as the original, medium or thumb rendition.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_photos_download(&self, Parameters(p): Parameters<PhotosDownloadParams>) -> ToolResult {
        let s = self.web().await?;
        s.photos_ready().await?;
        let version = p.version.as_deref().map(str::trim).filter(|v| !v.is_empty()).unwrap_or("original").to_lowercase();
        if !matches!(version.as_str(), "original" | "medium" | "thumb") {
            return Err(ToolError::new(format!("unknown version `{version}`: use original, medium or thumb")));
        }
        let wanted = p.filename.trim();
        if wanted.is_empty() {
            return Err(ToolError::new("filename is required"));
        }
        let lw = wanted.to_lowercase();
        let album = s.resolve_album(p.album.as_deref()).await?;
        let mut found: Option<(Value, Value)> = None;
        s.photos_scan(&album, 0, |m, a| {
            let id = a["recordName"].as_str().unwrap_or("");
            if filename_of(&m, id).to_lowercase() == lw {
                found = Some((m, a));
                return false;
            }
            true
        })
        .await?;
        let (master, asset) = found.ok_or_else(|| ToolError::new(format!("no item named `{wanted}` in album `{}`", album.fullname)))?;
        let id = asset["recordName"].as_str().unwrap_or("");
        let filename = filename_of(&master, id);
        let kind = item_type(&master, &filename);
        let prefix = version_prefix(kind, &version).ok_or_else(|| ToolError::new(format!("version `{version}` not defined for a {kind}")))?;
        let res = &master["fields"][format!("{prefix}Res")]["value"];
        let url = res["downloadURL"].as_str().ok_or_else(|| ToolError::new(format!("`{filename}` has no {version} rendition")))?;
        let file_type = master["fields"][format!("{prefix}FileType")]["value"].as_str().unwrap_or("");
        let local_name = resource_filename(&filename, file_type);
        let bytes = s.photos_download(url).await?;
        let dest = write_local(&p.destination, &local_name, &bytes, p.overwrite.unwrap_or(false))?;
        Ok(render(&json!({
            "path": dest.display().to_string(),
            "bytes": bytes.len(),
            "filename": filename,
            "version": version,
            "type": kind,
            "file_type": file_type,
            "expected_size": res["size"],
            "created": ts(&asset["fields"]["assetDate"]),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_name_decoding() {
        assert_eq!(decode_enc(&json!({"type": "ENCRYPTED_BYTES", "value": "VmFjYW5jZXMgw6AgUGFyaXM="})).as_deref(), Some("Vacances à Paris"));
        assert_eq!(decode_enc(&json!({"type": "STRING", "value": "Plain name!"})).as_deref(), Some("Plain name!"));
        assert!(decode_enc(&json!({})).is_none());
    }

    #[test]
    fn smart_album_count_ids() {
        let smart = smart_albums();
        assert_eq!(smart[0].name, "Library");
        assert!(smart[0].descending);
        assert_eq!(smart[0].count_id(), "CPLAssetByAssetDateWithoutHiddenOrDeleted");
        let fav = smart.iter().find(|a| a.name == "Favorites").unwrap();
        assert_eq!(fav.count_id(), "CPLAssetInSmartAlbumByAssetDate:Favorite");
        assert_eq!(fav.filters[0]["fieldValue"]["value"], "FAVORITE");
        assert!(!fav.descending);
        let user = Album {
            id: "ABC".into(),
            name: "x".into(),
            fullname: "x".into(),
            kind: "album",
            obj_type: "CPLContainerRelationNotDeletedByAssetDate".into(),
            list_type: "CPLContainerRelationLiveByAssetDate".into(),
            descending: false,
            filters: vec![],
        };
        assert_eq!(user.count_id(), "CPLContainerRelationNotDeletedByAssetDate:ABC");
    }

    #[test]
    fn item_types_and_versions() {
        let m = json!({"fields": {"itemType": {"value": "public.heic"}}});
        assert_eq!(item_type(&m, "x"), "image");
        let m = json!({"fields": {"resOriginalFileType": {"value": "com.apple.quicktime-movie"}}});
        assert_eq!(item_type(&m, "x"), "movie");
        assert_eq!(item_type(&json!({"fields": {"resOriginalFileType": {"value": "com.foo.raw-thing"}}}), "x"), "image");
        assert_eq!(item_type(&json!({"fields": {}}), "IMG_1.CR3"), "image");
        assert_eq!(item_type(&json!({"fields": {}}), "clip.mov"), "movie");
        assert_eq!(version_prefix("image", "medium"), Some("resJPEGMed"));
        assert_eq!(version_prefix("image", "thumb"), Some("resJPEGThumb"));
        assert_eq!(version_prefix("movie", "medium"), Some("resVidMed"));
        assert_eq!(version_prefix("movie", "thumb"), Some("resVidSmall"));
        assert_eq!(version_prefix("movie", "original"), Some("resOriginal"));
        assert_eq!(version_prefix("movie", "sidecar"), None);
    }

    #[test]
    fn resource_filenames() {
        assert_eq!(resource_filename("IMG_0001.HEIC", "public.jpeg"), "IMG_0001.JPG");
        assert_eq!(resource_filename("IMG_0001.HEIC", "com.apple.quicktime-movie"), "IMG_0001.MOV");
        assert_eq!(resource_filename("IMG_0001.HEIC", "com.unknown.type"), "IMG_0001.HEIC");
        assert_eq!(resource_filename(".hidden", "public.png"), ".hidden.PNG");
    }

    #[test]
    fn pairing_masters_and_assets() {
        let recs = vec![
            json!({"recordType": "CPLAsset", "recordName": "A1", "fields": {"masterRef": {"value": {"recordName": "M1"}}}}),
            json!({"recordType": "CPLMaster", "recordName": "M2", "fields": {}}),
            json!({"recordType": "CPLMaster", "recordName": "M1", "fields": {"filenameEnc": {"value": "SU1HXzEuSEVJQw=="}}}),
            json!({"recordType": "CPLAsset", "recordName": "A2", "fields": {"masterRef": {"value": {"recordName": "M1"}}}}),
            json!({"recordType": "CPLAsset", "recordName": "A3", "fields": {"masterRef": {"value": {"recordName": "M2"}}}}),
            json!({"recordType": "Other", "recordName": "X"}),
        ];
        let pairs = group_pairs(&recs);
        let ids: Vec<&str> = pairs.iter().map(|(_, a)| a["recordName"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["A3", "A1", "A2"]);
        assert_eq!(filename_of(&pairs[1].0, "A1"), "IMG_1.HEIC");
        assert_eq!(filename_of(&pairs[0].0, "A3"), "A3");
        let j = photo_json(&pairs[1].0, &json!({"recordName": "A1", "fields": {"assetDate": {"value": 1700000000000i64}, "isFavorite": {"value": 1}}}));
        assert_eq!(j["created"], "2023-11-14T22:13:20Z");
        assert_eq!(j["favorite"], true);
        assert_eq!(j["type"], "image");
    }

    #[test]
    fn timestamp_sentinels() {
        assert!(ts(&json!({"value": NULL_TS})).is_none());
        assert!(ts(&json!({"value": NULL_TS2})).is_none());
        assert_eq!(ts(&json!({"value": 0})).as_deref(), Some("1970-01-01T00:00:00Z"));
    }
}
