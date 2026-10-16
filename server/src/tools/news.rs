//! Apple News Publisher API (HMAC-SHA256 "HHMAC" authentication).

use base64::prelude::*;
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;

use crate::error::{ToolError, ToolResult};
use crate::http::{self, render};
use crate::server::AppleServer;

const SECTION: &str = "news";
const BASE: &str = "https://news-api.apple.com";

impl AppleServer {
    fn news_auth_header(&self, method: &str, url: &str, date: &str, content_type: Option<&str>, body: &[u8]) -> Result<String, ToolError> {
        let cfg = self.cfg();
        let key_id = cfg.require(SECTION, "key_id")?;
        let secret = cfg.require(SECTION, "key_secret")?;
        let key = BASE64_STANDARD.decode(secret.trim()).map_err(|e| ToolError::new(format!("news.key_secret must be base64: {e}")))?;
        let mut canonical = format!("{}{}{}", method.to_uppercase(), url, date).into_bytes();
        if let Some(ct) = content_type {
            canonical.extend_from_slice(ct.as_bytes());
            canonical.extend_from_slice(body);
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|e| ToolError::new(e.to_string()))?;
        mac.update(&canonical);
        let sig = BASE64_STANDARD.encode(mac.finalize().into_bytes());
        Ok(format!("HHMAC; key={key_id}; signature={sig}; date={date}"))
    }

    pub(crate) async fn news_call(&self, method: &str, path: &str, body: Option<(String, Vec<u8>)>) -> Result<Value, ToolError> {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{BASE}/{}", path.trim_start_matches('/')) };
        let date = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let (ct, bytes) = match &body {
            Some((ct, b)) => (Some(ct.as_str()), b.as_slice()),
            None => (None, &[][..]),
        };
        let auth = self.news_auth_header(method, &url, &date, ct, bytes)?;
        let mut req = self.http.request(http::method(method)?, &url).header("Authorization", auth);
        if let Some((ct, b)) = body {
            req = req.header("Content-Type", ct).body(b);
        }
        http::json(req, &format!("Apple News {} {}", method.to_uppercase(), path)).await
    }

    fn news_channel_id(&self) -> Result<String, ToolError> {
        self.cfg().require(SECTION, "channel_id")
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NewsRequestParams {
    /// GET, POST, DELETE
    pub method: String,
    /// Path, e.g. `/channels/{channelId}`, `/articles/{articleId}`, `/channels/{channelId}/sections`
    pub path: String,
    /// Raw body for POST (string); multipart article uploads use news_publish_article instead
    pub body: Option<String>,
    /// Content-Type for the body
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NewsArticlesParams {
    /// Page size (default 10, max 100)
    pub limit: Option<u32>,
    /// Sort direction: DESC (default) or ASC by createdAt
    pub sort: Option<String>,
    /// pageToken from a previous response
    pub page_token: Option<String>,
    /// Only articles created after this ISO 8601 date
    pub from_date: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NewsArticleParams {
    /// Article id
    pub article_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NewsPublishParams {
    /// Apple News Format article.json content (object)
    pub article: Value,
    /// Optional metadata object, e.g. {"data": {"isPreview": true, "links": {"sections": ["https://news-api.apple.com/sections/..."]}}}
    pub metadata: Option<Value>,
    /// If set, update this existing article id instead of creating (metadata.data.revision is required by Apple for updates)
    pub article_id: Option<String>,
}

#[tool_router(router = news_router, vis = "pub")]
impl AppleServer {
    /// Generic Apple News Publisher API request (channel, sections, articles, search).
    #[tool(annotations(open_world_hint = true))]
    pub async fn news_request(&self, Parameters(p): Parameters<NewsRequestParams>) -> ToolResult {
        let body = p.body.as_ref().map(|b| (p.content_type.clone().unwrap_or_else(|| "application/json".into()), b.clone().into_bytes()));
        Ok(render(&self.news_call(&p.method, &p.path, body).await?))
    }

    /// Channel info (name, website, share URL, quota) for the configured Apple News channel.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn news_channel(&self) -> ToolResult {
        let ch = self.news_channel_id()?;
        Ok(render(&self.news_call("GET", &format!("/channels/{ch}"), None).await?))
    }

    /// List articles of the channel (title, state, dates, share URL), newest first.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn news_articles(&self, Parameters(p): Parameters<NewsArticlesParams>) -> ToolResult {
        let ch = self.news_channel_id()?;
        let mut q = vec![("pageSize".to_string(), p.limit.unwrap_or(10).clamp(1, 100).to_string())];
        crate::util::opt(&mut q, "sortDir", &p.sort);
        crate::util::opt(&mut q, "pageToken", &p.page_token);
        crate::util::opt(&mut q, "fromDate", &p.from_date);
        let qs: String = q.iter().map(|(k, v)| format!("{k}={}", urlencode(v))).collect::<Vec<_>>().join("&");
        Ok(render(&self.news_call("GET", &format!("/channels/{ch}/articles?{qs}"), None).await?))
    }

    /// Get one article's metadata and state.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn news_article(&self, Parameters(p): Parameters<NewsArticleParams>) -> ToolResult {
        Ok(render(&self.news_call("GET", &format!("/articles/{}", p.article_id), None).await?))
    }

    /// List the channel's sections (ids and names) — needed to publish an article into a section.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn news_sections(&self) -> ToolResult {
        let ch = self.news_channel_id()?;
        Ok(render(&self.news_call("GET", &format!("/channels/{ch}/sections"), None).await?))
    }

    /// Publish (create) or update an Apple News Format article in the channel via multipart upload.
    #[tool(annotations(open_world_hint = true))]
    pub async fn news_publish_article(&self, Parameters(p): Parameters<NewsPublishParams>) -> ToolResult {
        let ch = self.news_channel_id()?;
        let boundary = format!("----apple-mcp-{}", uuid::Uuid::new_v4().simple());
        let mut body: Vec<u8> = Vec::new();
        let article_json = serde_json::to_vec(&p.article)?;
        if let Some(meta) = &p.metadata {
            let meta_json = serde_json::to_vec(meta)?;
            body.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json\r\nContent-Disposition: form-data; name=metadata\r\n\r\n").as_bytes());
            body.extend_from_slice(&meta_json);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json\r\nContent-Disposition: form-data; filename=article.json; name=article.json; size={}\r\n\r\n", article_json.len()).as_bytes());
        body.extend_from_slice(&article_json);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let ct = format!("multipart/form-data; boundary={boundary}");
        let path = match &p.article_id {
            Some(id) => format!("/articles/{id}"),
            None => format!("/channels/{ch}/articles"),
        };
        Ok(render(&self.news_call("POST", &path, Some((ct, body))).await?))
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
