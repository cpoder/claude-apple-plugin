//! Apple Music API: catalog search/lookup/charts and (with a Music User Token) the user's library.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, clip, query_pairs, render};
use crate::jwt;
use crate::server::AppleServer;
use crate::util::{opt, opt_num};

const SECTION: &str = "music";
const BASE: &str = "https://api.music.apple.com";

impl AppleServer {
    fn music_token(&self) -> Result<String, ToolError> {
        let cfg = self.cfg();
        let key_id = cfg.require(SECTION, "key_id")?;
        let team = cfg.team_id(SECTION)?;
        let pem = cfg.private_key(SECTION)?;
        self.tokens.get_or_mint("music", Duration::from_secs(11 * 3600), || {
            let now = jwt::now();
            jwt::es256_jwt(&pem, json!({"kid": key_id}), json!({"iss": team, "iat": now, "exp": now + 12 * 3600}))
        })
    }

    pub(crate) fn music_storefront(&self, p: &Option<String>) -> String {
        p.clone().unwrap_or_else(|| self.cfg().get_str_or(SECTION, "storefront", "us")).to_lowercase()
    }

    pub(crate) async fn music_call(&self, method: &str, path: &str, query: &[(String, String)], body: Option<&Value>, compact: bool) -> Result<Value, ToolError> {
        let token = self.music_token()?;
        let url = if path.starts_with("http") { path.to_string() } else { format!("{BASE}/{}", path.trim_start_matches('/')) };
        let mut req = self.http.request(http::method(method)?, &url).bearer_auth(token);
        if path.contains("/me/") || path.contains("/me?") {
            match self.cfg().get_str(SECTION, "user_token") {
                Some(ut) => req = req.header("Music-User-Token", ut),
                None => crate::bail!("`{path}` needs a Music User Token (config music.user_token). Obtain one with MusicKit JS (MusicKit.getInstance().authorize()) or MusicKit on iOS/macOS."),
            }
        }
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let mut v = http::json(req, &format!("Apple Music {} {}", method.to_uppercase(), path)).await?;
        if compact {
            compact_music(&mut v);
        }
        Ok(v)
    }
}

/// Trim verbose Apple Music attributes (artwork → url, drop playParams/previews/flags).
pub fn compact_music(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Object(attrs)) = map.get_mut("attributes") {
                for k in ["playParams", "previews", "audioTraits", "audioLocale", "hasTimeSyncedLyrics", "isAppleDigitalMaster", "hasLyrics", "isMasteredForItunes", "isVocalAttenuationAllowed", "attribution", "editorialVideo", "editorialArtwork", "plainEditorialNotes", "artistUrl"] {
                    attrs.remove(k);
                }
                if let Some(art) = attrs.get("artwork").and_then(|a| a.get("url")).and_then(|u| u.as_str()) {
                    let url = art.replace("{w}", "300").replace("{h}", "300");
                    attrs.insert("artwork".into(), Value::String(url));
                }
                if let Some(Value::Object(notes)) = attrs.get_mut("editorialNotes") {
                    let short = notes.get("short").or_else(|| notes.get("standard")).and_then(|s| s.as_str()).map(|s| clip(&crate::util::strip_html(s), 240)).unwrap_or_default();
                    attrs.insert("editorialNotes".into(), Value::String(short));
                }
                if let Some(Value::String(d)) = attrs.get("description").cloned() {
                    attrs.insert("description".into(), Value::String(clip(&d, 240)));
                }
            }
            map.remove("href");
            for (_, child) in map.iter_mut() {
                compact_music(child);
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(compact_music),
        _ => {}
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicRequestParams {
    /// GET, POST, PUT, DELETE
    pub method: String,
    /// Path, e.g. `/v1/catalog/fr/songs/123`, `/v1/me/library/playlists`, `/v1/storefronts`
    pub path: String,
    /// Query params, e.g. {"include": "artists", "limit": 25, "l": "fr-FR"}
    pub query: Option<serde_json::Map<String, Value>>,
    /// JSON body for POST/PUT
    pub body: Option<Value>,
    /// Trim verbose attributes (default true)
    pub compact: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicSearchParams {
    /// Search text
    pub term: String,
    /// Comma-separated types: songs, albums, artists, playlists, music-videos, stations, curators, record-labels (default songs,albums,artists,playlists)
    pub types: Option<String>,
    /// Storefront (default from config or us), e.g. fr
    pub storefront: Option<String>,
    /// 1-25 results per type (default 5)
    pub limit: Option<u32>,
    /// Localization, e.g. fr-FR
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicGetParams {
    /// Catalog resource type: songs, albums, artists, playlists, music-videos, stations, curators, apple-curators, genres, record-labels, activities
    pub resource_type: String,
    /// Resource id (or comma-separated ids for a multi-get; then relationship must be empty)
    pub id: String,
    /// Optional relationship to fetch instead, e.g. tracks (albums/playlists), albums (artists), songs, top-songs (artists view)
    pub relationship: Option<String>,
    /// Comma-separated relationships to include, e.g. artists,albums
    pub include: Option<String>,
    /// Comma-separated views, e.g. top-songs,latest-release (artists), other-versions (albums)
    pub views: Option<String>,
    /// Storefront (default from config)
    pub storefront: Option<String>,
    /// Localization, e.g. fr-FR
    pub lang: Option<String>,
    /// Limit for relationship/view results
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicChartsParams {
    /// Comma-separated: songs, albums, playlists, music-videos (default songs,albums)
    pub types: Option<String>,
    /// Storefront (default from config)
    pub storefront: Option<String>,
    /// Genre id to filter (e.g. 21 = Rock, 14 = Pop)
    pub genre: Option<String>,
    /// Chart name (default most-played); city charts: e.g. "city-chart-paris"
    pub chart: Option<String>,
    /// 1-200 (default 10)
    pub limit: Option<u32>,
    /// Localization, e.g. fr-FR
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicLibraryParams {
    /// songs | albums | artists | playlists | music-videos | recently-added | recently-played | recommendations | heavy-rotation | storefront | playlist-tracks (needs id)
    pub kind: String,
    /// Library playlist/album id for playlist-tracks / album-tracks
    pub id: Option<String>,
    /// 1-100 (default 25)
    pub limit: Option<u32>,
    /// Pagination offset
    pub offset: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicLibraryAddParams {
    /// Catalog resource type to add: songs, albums, playlists, music-videos
    pub resource_type: String,
    /// Catalog ids (comma-separated)
    pub ids: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicPlaylistCreateParams {
    /// Playlist name
    pub name: String,
    /// Optional description
    pub description: Option<String>,
    /// Catalog song ids to add initially (comma-separated)
    pub track_ids: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MusicPlaylistAddParams {
    /// Library playlist id (p.xxxx)
    pub playlist_id: String,
    /// Catalog song ids to append (comma-separated)
    pub track_ids: String,
    /// Track type (default songs; or library-songs for library ids)
    pub track_type: Option<String>,
}

#[tool_router(router = music_router, vis = "pub")]
impl AppleServer {
    /// Generic Apple Music API request (catalog, storefronts, charts, search hints, library with user token).
    #[tool(annotations(open_world_hint = true))]
    pub async fn music_request(&self, Parameters(p): Parameters<MusicRequestParams>) -> ToolResult {
        Ok(render(&self.music_call(&p.method, &p.path, &query_pairs(&p.query), p.body.as_ref(), p.compact.unwrap_or(true)).await?))
    }

    /// Search the Apple Music catalog (songs, albums, artists, playlists...) in a storefront.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn music_search(&self, Parameters(p): Parameters<MusicSearchParams>) -> ToolResult {
        let sf = self.music_storefront(&p.storefront);
        let mut q = vec![("term".to_string(), p.term.clone()), ("types".to_string(), p.types.clone().unwrap_or_else(|| "songs,albums,artists,playlists".into())), ("limit".to_string(), p.limit.unwrap_or(5).clamp(1, 25).to_string())];
        opt(&mut q, "l", &p.lang);
        let v = self.music_call("GET", &format!("/v1/catalog/{sf}/search"), &q, None, true).await?;
        // flatten: results.{type}.data[] → {type: [...]}
        let mut out = serde_json::Map::new();
        if let Some(results) = v["results"].as_object() {
            for (ty, r) in results {
                let items: Vec<Value> = r["data"].as_array().cloned().unwrap_or_default().iter().map(|d| {
                    let a = &d["attributes"];
                    json!({"id": d["id"], "name": a["name"], "artistName": a["artistName"], "albumName": a["albumName"], "releaseDate": a["releaseDate"], "genreNames": a["genreNames"], "url": a["url"], "curatorName": a["curatorName"], "trackCount": a["trackCount"], "durationInMillis": a["durationInMillis"], "isrc": a["isrc"]})
                }).collect();
                out.insert(ty.clone(), Value::Array(items));
            }
        }
        Ok(render(&json!({"storefront": sf, "results": out})))
    }

    /// Fetch a catalog resource by id (song, album with tracks, artist with albums/top songs, playlist, station, genre...).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn music_get(&self, Parameters(p): Parameters<MusicGetParams>) -> ToolResult {
        let sf = self.music_storefront(&p.storefront);
        let mut q: Vec<(String, String)> = Vec::new();
        opt(&mut q, "include", &p.include);
        opt(&mut q, "views", &p.views);
        opt(&mut q, "l", &p.lang);
        opt_num(&mut q, "limit", &p.limit);
        let path = if p.id.contains(',') {
            q.push(("ids".into(), p.id.clone()));
            format!("/v1/catalog/{sf}/{}", p.resource_type)
        } else {
            match &p.relationship {
                Some(rel) if !rel.is_empty() => format!("/v1/catalog/{sf}/{}/{}/{rel}", p.resource_type, p.id),
                _ => format!("/v1/catalog/{sf}/{}/{}", p.resource_type, p.id),
            }
        };
        Ok(render(&self.music_call("GET", &path, &q, None, true).await?))
    }

    /// Apple Music charts (most played songs/albums/playlists) for a storefront, optionally by genre or city chart.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn music_charts(&self, Parameters(p): Parameters<MusicChartsParams>) -> ToolResult {
        let sf = self.music_storefront(&p.storefront);
        let mut q = vec![("types".to_string(), p.types.clone().unwrap_or_else(|| "songs,albums".into())), ("limit".to_string(), p.limit.unwrap_or(10).clamp(1, 200).to_string())];
        opt(&mut q, "genre", &p.genre);
        opt(&mut q, "chart", &p.chart);
        opt(&mut q, "l", &p.lang);
        Ok(render(&self.music_call("GET", &format!("/v1/catalog/{sf}/charts"), &q, None, true).await?))
    }

    /// The user's library and personalized data (requires music.user_token): songs, albums, playlists, recently played, recommendations, heavy rotation.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn music_library(&self, Parameters(p): Parameters<MusicLibraryParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(25).clamp(1, 100).to_string();
        let mut q = vec![("limit".to_string(), limit)];
        opt_num(&mut q, "offset", &p.offset);
        let path = match p.kind.to_lowercase().as_str() {
            "songs" | "albums" | "artists" | "playlists" | "music-videos" => format!("/v1/me/library/{}", p.kind.to_lowercase()),
            "recently-added" => "/v1/me/library/recently-added".into(),
            "recently-played" => "/v1/me/recent/played/tracks".into(),
            "recommendations" => "/v1/me/recommendations".into(),
            "heavy-rotation" => "/v1/me/history/heavy-rotation".into(),
            "storefront" => "/v1/me/storefront".into(),
            "playlist-tracks" => format!("/v1/me/library/playlists/{}/tracks", p.id.clone().ok_or_else(|| ToolError::new("id required"))?),
            "album-tracks" => format!("/v1/me/library/albums/{}/tracks", p.id.clone().ok_or_else(|| ToolError::new("id required"))?),
            other => crate::bail!("unknown kind `{other}`"),
        };
        Ok(render(&self.music_call("GET", &path, &q, None, true).await?))
    }

    /// Add catalog songs/albums/playlists to the user's library (requires music.user_token).
    #[tool(annotations(open_world_hint = true))]
    pub async fn music_library_add(&self, Parameters(p): Parameters<MusicLibraryAddParams>) -> ToolResult {
        let q = vec![(format!("ids[{}]", p.resource_type), p.ids.clone())];
        let v = self.music_call("POST", "/v1/me/library", &q, None, true).await?;
        Ok(format!("Added {} {} to library. {}", p.ids, p.resource_type, render(&v)))
    }

    /// Create a playlist in the user's library, optionally with initial catalog songs (requires music.user_token).
    #[tool(annotations(open_world_hint = true))]
    pub async fn music_playlist_create(&self, Parameters(p): Parameters<MusicPlaylistCreateParams>) -> ToolResult {
        let mut body = json!({"attributes": {"name": p.name}});
        if let Some(d) = &p.description { body["attributes"]["description"] = json!(d); }
        if let Some(ids) = &p.track_ids {
            let data: Vec<Value> = ids.split(',').map(|id| json!({"id": id.trim(), "type": "songs"})).collect();
            body["relationships"] = json!({"tracks": {"data": data}});
        }
        Ok(render(&self.music_call("POST", "/v1/me/library/playlists", &[], Some(&body), true).await?))
    }

    /// Append songs to a library playlist (requires music.user_token).
    #[tool(annotations(open_world_hint = true))]
    pub async fn music_playlist_add_tracks(&self, Parameters(p): Parameters<MusicPlaylistAddParams>) -> ToolResult {
        let ty = p.track_type.clone().unwrap_or_else(|| "songs".into());
        let data: Vec<Value> = p.track_ids.split(',').map(|id| json!({"id": id.trim(), "type": ty})).collect();
        let v = self.music_call("POST", &format!("/v1/me/library/playlists/{}/tracks", p.playlist_id), &[], Some(&json!({"data": data})), true).await?;
        Ok(format!("Tracks added to playlist {}. {}", p.playlist_id, render(&v)))
    }
}
