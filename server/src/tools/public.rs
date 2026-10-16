//! Public Apple endpoints that need no credentials: iTunes Search/Lookup, charts (RSS), system status,
//! developer documentation, newsroom/developer news.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, bounded, clip, render};
use crate::server::AppleServer;
use crate::tools::docs;
use crate::util::{opt, opt_num, strip_html};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ItunesSearchParams {
    /// Search text (app name, artist, podcast, book...)
    pub term: String,
    /// Media type: software (apps), music, podcast, movie, tvShow, audiobook, ebook, musicVideo, shortFilm, all (default: software)
    pub media: Option<String>,
    /// Entity within the media type, e.g. software|iPadSoftware|macSoftware, song|album|musicArtist, podcast|podcastEpisode, ebook
    pub entity: Option<String>,
    /// Two-letter store country (default us), e.g. fr, de, gb, jp
    pub country: Option<String>,
    /// Max results 1-200 (default 10)
    pub limit: Option<u32>,
    /// Language for results: en_us or ja_jp
    pub lang: Option<String>,
    /// Attribute to search, e.g. artistTerm, titleTerm, softwareDeveloper
    pub attribute: Option<String>,
    /// Return the raw iTunes JSON instead of the compact summary (default false)
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ItunesLookupParams {
    /// iTunes/App Store numeric id(s), comma-separated (trackId, collectionId or artistId)
    pub id: Option<String>,
    /// App bundle identifier, e.g. com.apple.Pages
    pub bundle_id: Option<String>,
    /// ISBN (books)
    pub isbn: Option<String>,
    /// UPC/EAN (albums)
    pub upc: Option<String>,
    /// AMG artist id(s)
    pub amg_artist_id: Option<String>,
    /// Two-letter store country (default us)
    pub country: Option<String>,
    /// Entity to return relative to the id, e.g. song or album for an artist id, software for a developer id
    pub entity: Option<String>,
    /// Max results (with entity lookups)
    pub limit: Option<u32>,
    /// Return the raw JSON (default false)
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChartsParams {
    /// apps | music | podcasts | books | audiobooks (default apps)
    pub media: Option<String>,
    /// Feed: apps → top-free|top-paid; music → most-played; podcasts → top|top-subscriber; books → top-free|top-paid; audiobooks → top (default per media)
    pub feed: Option<String>,
    /// Result type: apps → apps; music → songs|albums|playlists|music-videos; podcasts → podcasts|podcast-episodes|podcast-channels; books → books; audiobooks → audio-books
    pub result_type: Option<String>,
    /// Two-letter country (default us)
    pub country: Option<String>,
    /// 1-200 (default 10)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SystemStatusParams {
    /// consumer (iCloud, App Store, Apple Music...) or developer (App Store Connect, APNs, TestFlight, Xcode Cloud...). Default consumer.
    pub scope: Option<String>,
    /// Only list services with active/recent events (default true)
    pub only_issues: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsParams {
    /// Documentation path or URL, e.g. `swiftui/view`, `/documentation/weatherkitrestapi`, `appstoreconnectapi/list_apps`, or a developer.apple.com URL
    pub path: String,
    /// all (default) | overview (declaration + discussion only) | topics (topic/relationship lists only)
    pub section: Option<String>,
    /// Max characters of Markdown to return (default 20000)
    pub max_chars: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsIndexParams {
    /// Technology/framework slug as in URLs, e.g. swiftui, uikit, foundation, appstoreconnectapi, weatherkitrestapi, applemusicapi, cloudkit, usernotifications
    pub technology: String,
    /// Words to search in titles/paths (all must match, case-insensitive). Omit for the top-level outline.
    pub query: Option<String>,
    /// Max rows (default 60)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TechnologiesParams {
    /// Filter words matched against technology name and tags (case-insensitive)
    pub query: Option<String>,
    /// Max rows (default 80)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NewsParams {
    /// newsroom (Apple press releases) | developer (developer.apple.com/news) | releases (OS/Xcode beta & release announcements). Default developer.
    pub feed: Option<String>,
    /// Max items (default 15)
    pub limit: Option<u32>,
}

const ITUNES_KEEP: &[&str] = &[
    "wrapperType", "kind", "trackId", "collectionId", "artistId", "trackName", "collectionName", "artistName", "bundleId",
    "sellerName", "primaryGenreName", "genres", "price", "formattedPrice", "currency", "averageUserRating", "userRatingCount",
    "version", "releaseDate", "currentVersionReleaseDate", "minimumOsVersion", "fileSizeBytes", "trackViewUrl", "collectionViewUrl",
    "artistViewUrl", "artworkUrl100", "contentAdvisoryRating", "trackCount", "collectionPrice", "trackTimeMillis", "feedUrl",
    "releaseNotes", "description", "longDescription", "copyright", "country", "languageCodesISO2A", "features", "isGameCenterEnabled",
];

fn compact_itunes(v: &Value) -> Value {
    let results = v["results"].as_array().cloned().unwrap_or_default();
    let compact: Vec<Value> = results
        .iter()
        .map(|r| {
            let mut m = serde_json::Map::new();
            for k in ITUNES_KEEP {
                if let Some(val) = r.get(*k) {
                    let val = match (*k, val) {
                        ("description" | "releaseNotes" | "longDescription", Value::String(s)) => Value::String(clip(s, 400)),
                        _ => val.clone(),
                    };
                    m.insert(k.to_string(), val);
                }
            }
            if let Some(d) = r.get("supportedDevices").and_then(|d| d.as_array()) {
                m.insert("supportedDevicesCount".into(), Value::from(d.len()));
            }
            Value::Object(m)
        })
        .collect();
    json!({ "resultCount": v["resultCount"], "results": compact })
}

fn parse_jsonp(text: &str) -> Result<Value, ToolError> {
    let t = text.trim();
    let inner = if let Some(start) = t.find('(') {
        if t.starts_with("jsonCallback") || !t.starts_with('{') {
            let end = t.rfind(')').unwrap_or(t.len());
            &t[start + 1..end]
        } else {
            t
        }
    } else {
        t
    };
    serde_json::from_str(inner).map_err(|e| ToolError::new(format!("cannot parse status feed: {e}")))
}

fn parse_feed(xml: &str, limit: usize) -> Result<Vec<Value>, ToolError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| ToolError::new(format!("feed XML parse error: {e}")))?;
    let root = doc.root_element();
    let mut items = Vec::new();
    let is_atom = root.tag_name().name() == "feed";
    let entries = root.descendants().filter(|n| n.is_element() && n.tag_name().name() == if is_atom { "entry" } else { "item" });
    for e in entries.take(limit) {
        let child_text = |name: &str| e.children().find(|c| c.is_element() && c.tag_name().name() == name).and_then(|c| c.text()).map(|t| t.trim().to_string()).unwrap_or_default();
        let link = if is_atom {
            e.children().find(|c| c.is_element() && c.tag_name().name() == "link").and_then(|c| c.attribute("href")).unwrap_or("").to_string()
        } else {
            child_text("link")
        };
        let date = if is_atom { let u = child_text("updated"); if u.is_empty() { child_text("published") } else { u } } else { child_text("pubDate") };
        let summary = if is_atom { let s = child_text("summary"); if s.is_empty() { child_text("content") } else { s } } else { child_text("description") };
        items.push(json!({
            "title": strip_html(&child_text("title")),
            "link": link,
            "date": date,
            "summary": clip(&strip_html(&summary), 400),
        }));
    }
    Ok(items)
}

#[tool_router(router = public_router, vis = "pub")]
impl AppleServer {
    /// Search the App Store / iTunes catalog (apps, music, podcasts, books, movies) via the public iTunes Search API. No credentials needed. Rate limit ~20 calls/min.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn itunes_search(&self, Parameters(p): Parameters<ItunesSearchParams>) -> ToolResult {
        let mut q = vec![("term".to_string(), p.term.clone())];
        q.push(("media".into(), p.media.clone().unwrap_or_else(|| "software".into())));
        q.push(("country".into(), p.country.clone().unwrap_or_else(|| "us".into())));
        q.push(("limit".into(), p.limit.unwrap_or(10).clamp(1, 200).to_string()));
        opt(&mut q, "entity", &p.entity);
        opt(&mut q, "lang", &p.lang);
        opt(&mut q, "attribute", &p.attribute);
        let v = http::json(self.http.get("https://itunes.apple.com/search").query(&q), "iTunes search").await?;
        Ok(render(&if p.raw.unwrap_or(false) { v } else { compact_itunes(&v) }))
    }

    /// Look up App Store / iTunes items by id, bundle id, ISBN, UPC or AMG id (public iTunes Lookup API). Use entity to list an artist's albums or a developer's apps.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn itunes_lookup(&self, Parameters(p): Parameters<ItunesLookupParams>) -> ToolResult {
        let mut q: Vec<(String, String)> = Vec::new();
        opt(&mut q, "id", &p.id);
        opt(&mut q, "bundleId", &p.bundle_id);
        opt(&mut q, "isbn", &p.isbn);
        opt(&mut q, "upc", &p.upc);
        opt(&mut q, "amgArtistId", &p.amg_artist_id);
        if q.is_empty() {
            crate::bail!("provide one of id, bundle_id, isbn, upc, amg_artist_id");
        }
        q.push(("country".into(), p.country.clone().unwrap_or_else(|| "us".into())));
        opt(&mut q, "entity", &p.entity);
        opt_num(&mut q, "limit", &p.limit);
        let v = http::json(self.http.get("https://itunes.apple.com/lookup").query(&q), "iTunes lookup").await?;
        Ok(render(&if p.raw.unwrap_or(false) { v } else { compact_itunes(&v) }))
    }

    /// Top charts from Apple's public RSS feed generator: top free/paid apps, most-played songs/albums, top podcasts, books, audiobooks, per country.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn apple_charts(&self, Parameters(p): Parameters<ChartsParams>) -> ToolResult {
        let media = p.media.clone().unwrap_or_else(|| "apps".into()).to_lowercase();
        let (feed_default, type_default) = match media.as_str() {
            "apps" => ("top-free", "apps"),
            "music" => ("most-played", "songs"),
            "podcasts" => ("top", "podcasts"),
            "books" => ("top-free", "books"),
            "audiobooks" | "audio-books" => ("top", "audio-books"),
            other => crate::bail!("unknown media `{other}` (apps|music|podcasts|books|audiobooks)"),
        };
        let media_path = if media == "audiobooks" { "audio-books".to_string() } else { media.clone() };
        let feed = p.feed.clone().unwrap_or_else(|| feed_default.into());
        let rtype = p.result_type.clone().unwrap_or_else(|| type_default.into());
        let country = p.country.clone().unwrap_or_else(|| "us".into()).to_lowercase();
        let limit = p.limit.unwrap_or(10).clamp(1, 200);
        let url = format!("https://rss.marketingtools.apple.com/api/v2/{country}/{media_path}/{feed}/{limit}/{rtype}.json");
        let resp = http::send(self.http.get(&url)).await?;
        if !resp.ok() {
            crate::bail!("charts feed {url}: HTTP {} (feed combination may not exist, or the feed service timed out — retry)", resp.status);
        }
        let v = resp.json().ok_or_else(|| ToolError::new("charts feed returned non-JSON"))?;
        let feed_obj = &v["feed"];
        let results: Vec<Value> = feed_obj["results"].as_array().cloned().unwrap_or_default().iter().enumerate().map(|(i, r)| {
            let mut m = serde_json::Map::new();
            m.insert("rank".into(), Value::from(i + 1));
            for k in ["name", "artistName", "id", "url", "releaseDate", "kind", "artworkUrl100"] {
                if let Some(val) = r.get(k) { m.insert(k.into(), val.clone()); }
            }
            if let Some(g) = r.get("genres").and_then(|g| g.as_array()) {
                m.insert("genres".into(), Value::Array(g.iter().filter_map(|x| x.get("name").cloned()).collect()));
            }
            Value::Object(m)
        }).collect();
        Ok(render(&json!({
            "title": feed_obj["title"], "country": feed_obj["country"], "updated": feed_obj["updated"], "url": url, "results": results
        })))
    }

    /// Apple System Status: current outages/incidents for consumer services (iCloud, App Store, Apple Music, Maps...) or developer services (App Store Connect, APNs, TestFlight, Xcode Cloud, CloudKit...). Public, no credentials.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn apple_system_status(&self, Parameters(p): Parameters<SystemStatusParams>) -> ToolResult {
        let scope = p.scope.clone().unwrap_or_else(|| "consumer".into()).to_lowercase();
        let url = match scope.as_str() {
            "developer" | "dev" => "https://www.apple.com/support/systemstatus/data/developer/system_status_en_US.js",
            _ => "https://www.apple.com/support/systemstatus/data/system_status_en_US.js",
        };
        let resp = http::send(self.http.get(url)).await?;
        if !resp.ok() {
            crate::bail!("system status: HTTP {}", resp.status);
        }
        let v = parse_jsonp(&resp.text())?;
        let services = v["services"].as_array().cloned().unwrap_or_default();
        let only_issues = p.only_issues.unwrap_or(true);
        let mut issues = Vec::new();
        let mut ok_names = Vec::new();
        for svc in &services {
            let name = svc["serviceName"].as_str().unwrap_or("").to_string();
            let events = svc["events"].as_array().cloned().unwrap_or_default();
            if events.is_empty() {
                ok_names.push(name);
            } else {
                let evs: Vec<Value> = events.iter().map(|e| json!({
                    "eventStatus": e["eventStatus"], "statusType": e["statusType"], "message": e["message"],
                    "startDate": e["startDate"], "endDate": e["endDate"], "usersAffected": e["usersAffected"], "affectedServices": e["affectedServices"],
                })).collect();
                issues.push(json!({"service": name, "events": evs}));
            }
        }
        let mut out = json!({
            "scope": scope,
            "services_total": services.len(),
            "services_with_events": issues.len(),
            "issues": issues,
        });
        if !only_issues {
            out["services_ok"] = Value::Array(ok_names.into_iter().map(Value::String).collect());
        }
        Ok(render(&out))
    }

    /// Fetch an Apple Developer Documentation page (frameworks, APIs, REST endpoints, articles) and render it as Markdown: declaration, availability, discussion, parameters, topics. Public.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn apple_docs(&self, Parameters(p): Parameters<DocsParams>) -> ToolResult {
        let path = docs::normalize_path(&p.path);
        if path.is_empty() {
            crate::bail!("empty documentation path");
        }
        let url = format!("https://developer.apple.com/tutorials/data/documentation/{path}.json");
        let resp = http::send(self.http.get(&url)).await?;
        if resp.status == 404 {
            crate::bail!("no documentation page at /documentation/{path} — use apple_docs_index to find the exact path (paths are lowercase, e.g. swiftui/view)");
        }
        if !resp.ok() {
            crate::bail!("docs fetch {url}: HTTP {}", resp.status);
        }
        let v = resp.json().ok_or_else(|| ToolError::new("documentation response was not JSON"))?;
        let section = p.section.clone().unwrap_or_else(|| "all".into()).to_lowercase();
        Ok(docs::render_doc(&v, &section, p.max_chars.unwrap_or(20_000) as usize))
    }

    /// Browse/search the table of contents of an Apple developer technology (e.g. swiftui, uikit, appstoreconnectapi, weatherkitrestapi): returns matching page titles with their documentation paths for apple_docs.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn apple_docs_index(&self, Parameters(p): Parameters<DocsIndexParams>) -> ToolResult {
        let tech = p.technology.trim().trim_matches('/').to_lowercase();
        let tech = tech.rsplit('/').next().unwrap_or(&tech).to_string();
        let url = format!("https://developer.apple.com/tutorials/data/index/{tech}");
        let resp = http::send(self.http.get(&url)).await?;
        if resp.status == 404 {
            crate::bail!("no index for technology `{tech}` — check the slug with apple_technologies");
        }
        if !resp.ok() {
            crate::bail!("docs index {url}: HTTP {}", resp.status);
        }
        let v = resp.json().ok_or_else(|| ToolError::new("index response was not JSON"))?;
        let langs = &v["interfaceLanguages"];
        let tree = langs.get("swift").or_else(|| langs.get("occ")).or_else(|| langs.get("data")).cloned().unwrap_or(Value::Null);
        let limit = p.limit.unwrap_or(60).clamp(1, 500) as usize;
        let mut rows = Vec::new();
        match p.query.as_deref().map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty()) {
            None => {
                docs::flatten_index(&tree, &mut rows, 0, 2);
                let lines: Vec<String> = rows.iter().take(limit).map(|(t, p, ty)| format!("{ty:<10} {t}  `{}`", p.trim_start_matches("/documentation/"))).collect();
                Ok(bounded(format!("Outline of {tech} ({} top entries shown of {}; pass query to search deeper):\n{}", lines.len(), rows.len(), lines.join("\n"))))
            }
            Some(q) => {
                docs::flatten_index(&tree, &mut rows, 0, 64);
                let words: Vec<&str> = q.split_whitespace().collect();
                let mut hits: Vec<(u8, &(String, String, String))> = rows
                    .iter()
                    .filter_map(|r| {
                        let title = r.0.to_lowercase();
                        let path = r.1.to_lowercase();
                        if words.iter().all(|w| title.contains(w) || path.contains(w)) {
                            let rank = if title == q { 0 } else if title.starts_with(&q) { 1 } else if title.contains(&q) { 2 } else { 3 };
                            Some((rank, r))
                        } else {
                            None
                        }
                    })
                    .collect();
                hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.0.len().cmp(&b.1.0.len())));
                let total = hits.len();
                let lines: Vec<String> = hits.iter().take(limit).map(|(_, (t, p, ty))| format!("{ty:<10} {t}  `{}`", p.trim_start_matches("/documentation/"))).collect();
                if lines.is_empty() {
                    return Ok(format!("No entries matching `{q}` in {tech} ({} pages scanned).", rows.len()));
                }
                Ok(bounded(format!("{total} matches in {tech} (showing {}):\n{}", lines.len(), lines.join("\n"))))
            }
        }
    }

    /// List Apple developer technologies/frameworks (name, tags, documentation path) from the documentation catalog, optionally filtered. Use it to find the slug for apple_docs_index.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn apple_technologies(&self, Parameters(p): Parameters<TechnologiesParams>) -> ToolResult {
        let v = http::json(self.http.get("https://developer.apple.com/tutorials/data/documentation/technologies.json"), "technologies catalog").await?;
        let q = p.query.as_deref().map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty());
        let limit = p.limit.unwrap_or(80).clamp(1, 1000) as usize;
        let mut lines = Vec::new();
        let mut total = 0usize;
        if let Some(sections) = v["sections"].as_array() {
            for sec in sections.iter().filter(|s| s["kind"] == "technologies") {
                for group in sec["groups"].as_array().cloned().unwrap_or_default() {
                    let gname = group["name"].as_str().unwrap_or("");
                    for t in group["technologies"].as_array().cloned().unwrap_or_default() {
                        let title = t["title"].as_str().unwrap_or("").to_string();
                        let tags: Vec<String> = t["tags"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
                        let ident = t["destination"]["identifier"].as_str().unwrap_or("");
                        let path = ident.find("/documentation/").map(|i| ident[i + "/documentation/".len()..].to_lowercase()).unwrap_or_default();
                        let hay = format!("{} {} {}", title.to_lowercase(), tags.join(" ").to_lowercase(), path);
                        if let Some(q) = &q {
                            if !q.split_whitespace().all(|w| hay.contains(w)) { continue; }
                        }
                        total += 1;
                        if lines.len() < limit {
                            lines.push(format!("{title}  `{path}`  [{gname}] {}", tags.join(", ")));
                        }
                    }
                }
            }
        }
        Ok(bounded(format!("{total} technologies{} (showing {}):\n{}", q.map(|q| format!(" matching `{q}`")).unwrap_or_default(), lines.len(), lines.join("\n"))))
    }

    /// Latest Apple news: press releases (newsroom), Apple Developer news, or OS/Xcode release announcements (releases). Public RSS feeds.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn apple_news(&self, Parameters(p): Parameters<NewsParams>) -> ToolResult {
        let feed = p.feed.clone().unwrap_or_else(|| "developer".into()).to_lowercase();
        let url = match feed.as_str() {
            "newsroom" | "press" => "https://www.apple.com/newsroom/rss-feed.rss",
            "releases" | "developer-releases" => "https://developer.apple.com/news/releases/rss/releases.rss",
            "developer" | "dev" => "https://developer.apple.com/news/rss/news.rss",
            other => crate::bail!("unknown feed `{other}` (newsroom|developer|releases)"),
        };
        let resp = http::send(self.http.get(url)).await?;
        if !resp.ok() {
            crate::bail!("news feed {url}: HTTP {}", resp.status);
        }
        let items = parse_feed(&resp.text(), p.limit.unwrap_or(15).clamp(1, 100) as usize)?;
        Ok(render(&json!({"feed": feed, "source": url, "items": items})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonp_variants() {
        assert_eq!(parse_jsonp(r#"jsonCallback({"a":1});"#).unwrap()["a"], 1);
        assert_eq!(parse_jsonp(r#"{"a":2}"#).unwrap()["a"], 2);
        assert_eq!(parse_jsonp(r#"{"a":"(x)"}"#).unwrap()["a"], "(x)");
    }

    #[test]
    fn parses_rss_and_atom() {
        let rss = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title><item><title>Hello &amp; bye</title><link>https://x/1</link><pubDate>Mon</pubDate><description><![CDATA[<p>Body</p>]]></description></item></channel></rss>"#;
        let items = parse_feed(rss, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "Hello & bye");
        assert_eq!(items[0]["link"], "https://x/1");
        assert_eq!(items[0]["summary"], "Body");
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>A</title><link href="https://x/a"/><updated>2026</updated><summary>S</summary></entry><entry><title>B</title></entry></feed>"#;
        let items = parse_feed(atom, 1).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["link"], "https://x/a");
    }

    #[test]
    fn compacts_itunes() {
        let v = json!({"resultCount": 1, "results": [{"trackName": "X", "description": "y".repeat(1000), "supportedDevices": ["a", "b"], "junk": 1}]});
        let c = compact_itunes(&v);
        assert_eq!(c["results"][0]["trackName"], "X");
        assert!(c["results"][0].get("junk").is_none());
        assert_eq!(c["results"][0]["supportedDevicesCount"], 2);
        assert!(c["results"][0]["description"].as_str().unwrap().len() < 410);
    }
}
