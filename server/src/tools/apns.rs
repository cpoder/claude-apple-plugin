//! Apple Push Notification service (token-based auth, HTTP/2).

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, render};
use crate::jwt;
use crate::server::AppleServer;

const SECTION: &str = "apns";

impl AppleServer {
    pub(crate) fn apns_token(&self) -> Result<String, ToolError> {
        let cfg = self.cfg();
        let key_id = cfg.require(SECTION, "key_id")?;
        let team = cfg.team_id(SECTION)?;
        let pem = cfg.private_key(SECTION)?;
        // Apple: refresh at most every 20 min, at least every 60 min.
        self.tokens.get_or_mint("apns", Duration::from_secs(45 * 60), || {
            jwt::es256_jwt(&pem, json!({"kid": key_id}), json!({"iss": team, "iat": jwt::now()}))
        })
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApnsSendParams {
    /// Device token (hex) or, for broadcast/channel pushes, use apns_request path semantics via topic
    pub device_token: String,
    /// Notification payload, e.g. {"aps": {"alert": {"title": "Hi", "body": "Hello"}, "sound": "default", "badge": 1}, "custom": "data"}
    pub payload: Value,
    /// alert (default), background, location, voip, complication, fileprovider, mdm, liveactivity, pushtotalk, widgets
    pub push_type: Option<String>,
    /// apns-topic (default config apns.topic = bundle id; suffixes like .voip/.complication/.push-type.liveactivity are appended automatically for those push types)
    pub topic: Option<String>,
    /// 10 (immediate, default for alert), 5 (power-considerate, required for background), 1
    pub priority: Option<u8>,
    /// Unix epoch seconds until which APNs retries delivery (0 = deliver once now)
    pub expiration: Option<i64>,
    /// Collapse identifier to coalesce notifications
    pub collapse_id: Option<String>,
    /// UUID for apns-id (generated if omitted)
    pub apns_id: Option<String>,
    /// production | sandbox (development builds use sandbox; default from config)
    pub environment: Option<String>,
}

#[tool_router(router = apns_router, vis = "pub")]
impl AppleServer {
    /// Send a push notification to a device token through APNs (HTTP/2, token-based auth). Returns the apns-id and any rejection reason (BadDeviceToken, TopicDisallowed...).
    #[tool(annotations(open_world_hint = true))]
    pub async fn apns_send(&self, Parameters(p): Parameters<ApnsSendParams>) -> ToolResult {
        let cfg = self.cfg();
        let token = self.apns_token()?;
        let env = p.environment.clone().unwrap_or_else(|| cfg.get_str_or(SECTION, "environment", "production")).to_lowercase();
        let host = if env.starts_with("sandbox") || env.starts_with("dev") { "https://api.sandbox.push.apple.com" } else { "https://api.push.apple.com" };
        let push_type = p.push_type.clone().unwrap_or_else(|| "alert".into()).to_lowercase();
        let base_topic = match &p.topic {
            Some(t) => t.clone(),
            None => cfg.require(SECTION, "topic")?,
        };
        let topic = if p.topic.is_some() {
            base_topic
        } else {
            match push_type.as_str() {
                "voip" => format!("{base_topic}.voip"),
                "complication" => format!("{base_topic}.complication"),
                "fileprovider" => format!("{base_topic}.pushkit.fileprovider"),
                "liveactivity" => format!("{base_topic}.push-type.liveactivity"),
                "location" => format!("{base_topic}.location-query"),
                "pushtotalk" => format!("{base_topic}.voip-ptt"),
                _ => base_topic,
            }
        };
        let apns_id = p.apns_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let priority = p.priority.unwrap_or(if push_type == "background" { 5 } else { 10 });
        let mut req = self
            .http
            .post(format!("{host}/3/device/{}", p.device_token.trim()))
            .header("authorization", format!("bearer {token}"))
            .header("apns-topic", &topic)
            .header("apns-push-type", &push_type)
            .header("apns-priority", priority.to_string())
            .header("apns-id", &apns_id)
            .json(&p.payload);
        if let Some(e) = p.expiration { req = req.header("apns-expiration", e.to_string()); }
        if let Some(c) = &p.collapse_id { req = req.header("apns-collapse-id", c); }
        let resp = http::send(req).await?;
        let returned_id = resp.header("apns-id").unwrap_or_default();
        if resp.ok() {
            Ok(render(&json!({"status": resp.status, "apns_id": returned_id, "topic": topic, "push_type": push_type, "environment": if host.contains("sandbox") { "sandbox" } else { "production" }})))
        } else {
            let body = resp.json().unwrap_or_else(|| json!({"body": resp.text()}));
            let reason = body.get("reason").and_then(|r| r.as_str()).unwrap_or("");
            let hint = match reason {
                "BadDeviceToken" => "token invalid for this environment (sandbox vs production) or malformed",
                "DeviceTokenNotForTopic" => "token belongs to another app (topic mismatch)",
                "TopicDisallowed" | "MissingTopic" => "apns-topic must be the app bundle id (plus suffix for special push types)",
                "InvalidProviderToken" | "ExpiredProviderToken" => "check key_id/team_id/private key; the key must have APNs enabled",
                "Unregistered" => "the device token is no longer active for the topic",
                "PayloadTooLarge" => "payload exceeds 4 KB (5 KB for VoIP)",
                _ => "",
            };
            crate::bail!("APNs rejected the notification: HTTP {} {} {}{}", resp.status, reason, body.get("timestamp").map(|t| format!("(timestamp {t})")).unwrap_or_default(), if hint.is_empty() { String::new() } else { format!(" — {hint}") })
        }
    }
}
