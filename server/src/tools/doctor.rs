//! Credential probes: one cheap, read-only call per service to prove a configuration works.

use std::time::Instant;

use serde_json::{Value, json};

use crate::config::{Config, SERVICES};
use crate::error::ToolError;
use crate::http;
use crate::server::AppleServer;

pub struct ProbeResult {
    pub section: &'static str,
    pub label: &'static str,
    pub configured: bool,
    pub ok: bool,
    pub detail: String,
    pub millis: u128,
}

impl ProbeResult {
    pub fn to_json(&self) -> Value {
        json!({"section": self.section, "service": self.label, "configured": self.configured, "ok": self.ok, "detail": self.detail, "ms": self.millis})
    }
}

impl AppleServer {
    /// Probe one service. Returns a human-readable detail string on success, an error on failure.
    pub async fn probe_service(&self, section: &str) -> Result<String, ToolError> {
        let cfg = self.cfg();
        match section {
            "app_store_connect" => {
                let v = self.asc_call("GET", "/v1/apps", &[("limit".into(), "1".into()), ("fields[apps]".into(), "name".into())], None, 1, true).await?;
                let n = v["meta"]["paging"]["total"].as_u64().map(|t| t.to_string()).unwrap_or_else(|| v["data"].as_array().map(|a| a.len().to_string()).unwrap_or_default());
                Ok(format!("App Store Connect OK — {n} app(s) visible to this key"))
            }
            "app_store_server" => {
                // A bogus order id yields HTTP 200 {status: 1} when auth is valid, 401 otherwise.
                let v = self.storekit_call("GET", "/inApps/v1/lookup/MMMMMMMMMM", &[], None, &None).await?;
                Ok(format!("App Store Server API OK ({}) — key, issuer and bundle id accepted (lookup status {})", v["_environment"].as_str().unwrap_or("?"), v["status"]))
            }
            "music" => {
                // /v1/storefronts answers 200 even without a token; a catalog search really validates the developer token.
                let sf = self.music_storefront(&None);
                let v = self.music_call("GET", &format!("/v1/catalog/{sf}/search"), &[("term".into(), "apple".into()), ("types".into(), "songs".into()), ("limit".into(), "1".into())], None, true).await?;
                let name = v["results"]["songs"]["data"][0]["attributes"]["name"].as_str().unwrap_or("search ok");
                let user = if cfg.get_str("music", "user_token").is_some() {
                    match self.music_call("GET", "/v1/me/storefront", &[], None, true).await {
                        Ok(_) => "; user token OK".to_string(),
                        Err(e) => format!("; user token FAILED: {e}"),
                    }
                } else {
                    String::new()
                };
                Ok(format!("Apple Music OK — developer token accepted (storefront {sf}, sample result: {name}){user}"))
            }
            "weatherkit" => {
                let v = self.weather_get("availability/48.8566/2.3522", &[("country".into(), "FR".into())]).await?;
                Ok(format!("WeatherKit OK — data sets for Paris: {}", v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_else(|| v.to_string())))
            }
            "maps" => {
                self.maps_access_token().await?;
                Ok("Apple Maps OK — access token obtained".into())
            }
            "cloudkit" => {
                let v = self.cloudkit_call("zones/list", &json!({}), &None, &None).await?;
                let zones: Vec<String> = v["zones"].as_array().map(|a| a.iter().filter_map(|z| z["zoneID"]["zoneName"].as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
                Ok(format!("CloudKit OK — zones: {}", zones.join(", ")))
            }
            "apns" => {
                // A malformed device token returns 400 BadDeviceToken when the provider token is accepted,
                // and 403 InvalidProviderToken/ExpiredProviderToken when it is not.
                let token = self.apns_token()?;
                let env = cfg.get_str_or("apns", "environment", "production");
                let host = if env.starts_with("sandbox") || env.starts_with("dev") { "https://api.sandbox.push.apple.com" } else { "https://api.push.apple.com" };
                let topic = cfg.require("apns", "topic")?;
                let resp = http::send(self.http.post(format!("{host}/3/device/00")).header("authorization", format!("bearer {token}")).header("apns-topic", &topic).header("apns-push-type", "alert").json(&json!({"aps": {"alert": "probe"}}))).await?;
                let reason = resp.json().and_then(|v| v["reason"].as_str().map(|s| s.to_string())).unwrap_or_default();
                match (resp.status, reason.as_str()) {
                    (400, "BadDeviceToken") => Ok(format!("APNs OK ({env}) — provider token accepted for topic {topic}")),
                    (400, "TopicDisallowed") | (400, "MissingTopic") => Err(ToolError::new(format!("APNs: provider token accepted but topic `{topic}` rejected ({reason})"))),
                    (s, r) => Err(ToolError::new(format!("APNs: HTTP {s} {r}"))),
                }
            }
            "sign_in_with_apple" => {
                let (_secret, client_id) = self.siwa_client_secret_jwt(1)?;
                http::json(self.http.get("https://appleid.apple.com/auth/keys"), "Apple public keys").await?;
                Ok(format!("Sign in with Apple OK — client secret minted for {client_id} (key file valid); token endpoints reachable"))
            }
            "business_manager" => {
                self.abm_access_token().await?;
                Ok(format!("Apple Business/School Manager OK — access token obtained (scope {})", cfg.get_str_or("business_manager", "scope", "business.api")))
            }
            "search_ads" => {
                let v = self.searchads_call("GET", "acls", &[], None, &None).await?;
                let orgs: Vec<String> = v["data"].as_array().map(|a| a.iter().filter_map(|o| Some(format!("{} ({})", o["orgName"].as_str()?, o["orgId"]))).collect()).unwrap_or_default();
                Ok(format!("Apple Search Ads OK — orgs: {}", orgs.join(", ")))
            }
            "news" => {
                let v = self.news_call("GET", &format!("/channels/{}", cfg.require("news", "channel_id")?), None).await?;
                Ok(format!("Apple News OK — channel {}", v["data"]["name"].as_str().unwrap_or("?")))
            }
            "icloud" => {
                let principal = self.principal(crate::tools::icloud_dav::CALDAV_ROOT).await?;
                let (user, pass) = self.mail_creds()?;
                let mail = tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
                    let mut s = crate::tools::icloud_mail::imap_connect(&user, &pass)?;
                    let m = s.examine("INBOX").map_err(|e| ToolError::new(e.to_string()))?;
                    let _ = s.logout();
                    Ok(format!("IMAP OK ({} messages in INBOX)", m.exists))
                })
                .await
                .map_err(|e| ToolError::new(e.to_string()))?;
                match mail {
                    Ok(m) => Ok(format!("iCloud OK — CalDAV principal {principal}; {m}")),
                    Err(e) => Ok(format!("iCloud partially OK — CalDAV principal {principal}; mail FAILED: {e}")),
                }
            }
            other => Err(ToolError::new(format!("unknown section `{other}`"))),
        }
    }

    /// Probe every configured service (or the given ones).
    pub async fn doctor(&self, only: Option<&[String]>) -> Vec<ProbeResult> {
        let cfg: Config = self.cfg();
        let mut results = Vec::new();
        for spec in SERVICES {
            if let Some(list) = only {
                if !list.iter().any(|s| s == spec.section) {
                    continue;
                }
            }
            let missing = cfg.missing(spec);
            if !missing.is_empty() {
                results.push(ProbeResult { section: spec.section, label: spec.label, configured: false, ok: false, detail: format!("not configured (missing: {})", missing.join(", ")), millis: 0 });
                continue;
            }
            let t0 = Instant::now();
            let (ok, detail) = match self.probe_service(spec.section).await {
                Ok(d) => (true, d),
                Err(e) => (false, e.to_string()),
            };
            results.push(ProbeResult { section: spec.section, label: spec.label, configured: true, ok, detail, millis: t0.elapsed().as_millis() });
        }
        results
    }
}
