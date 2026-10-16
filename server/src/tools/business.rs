//! Apple Business Manager / Apple School Manager API (OAuth 2 client credentials with JWT client assertion).

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, query_pairs, render, strip_links};
use crate::jwt;
use crate::server::AppleServer;

const SECTION: &str = "business_manager";
const TOKEN_URL: &str = "https://account.apple.com/auth/oauth2/token";
const ASSERTION_AUD: &str = "https://account.apple.com/auth/oauth2/v2/token";

impl AppleServer {
    fn abm_scope(&self) -> String {
        self.cfg().get_str_or(SECTION, "scope", "business.api")
    }

    fn abm_base(&self) -> String {
        if self.abm_scope().starts_with("school") { "https://api-school.apple.com/v1".into() } else { "https://api-business.apple.com/v1".into() }
    }

    pub(crate) async fn abm_access_token(&self) -> Result<String, ToolError> {
        if let Some(t) = self.tokens.get("abm") {
            return Ok(t);
        }
        let cfg = self.cfg();
        let client_id = cfg.require(SECTION, "client_id")?;
        let key_id = cfg.require(SECTION, "key_id")?;
        let pem = cfg.private_key(SECTION)?;
        let scope = self.abm_scope();
        let now = jwt::now();
        let assertion = jwt::es256_jwt(
            &pem,
            json!({"kid": key_id}),
            json!({"sub": client_id, "aud": ASSERTION_AUD, "iat": now, "exp": now + 3600, "jti": uuid::Uuid::new_v4().to_string(), "iss": client_id}),
        )?;
        let form = vec![
            ("grant_type".to_string(), "client_credentials".to_string()),
            ("client_id".to_string(), client_id),
            ("client_assertion_type".to_string(), "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_string()),
            ("client_assertion".to_string(), assertion),
            ("scope".to_string(), scope),
        ];
        let v = http::json(self.http.post(TOKEN_URL).form(&form), "Apple Business Manager token").await?;
        let token = v["access_token"].as_str().ok_or_else(|| ToolError::new(format!("no access_token in response: {v}")))?.to_string();
        let ttl = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60).max(60);
        self.tokens.put("abm", token.clone(), Duration::from_secs(ttl));
        Ok(token)
    }

    pub(crate) async fn abm_call(&self, method: &str, path: &str, query: &[(String, String)], body: Option<&Value>, max_pages: u32) -> Result<Value, ToolError> {
        let token = self.abm_access_token().await?;
        let base = self.abm_base();
        let mut url = if path.starts_with("http") { path.to_string() } else { format!("{base}/{}", path.trim_start_matches('/').trim_start_matches("v1/")) };
        let mut merged: Option<Value> = None;
        let mut pages = 0;
        loop {
            let mut req = self.http.request(http::method(method)?, &url).bearer_auth(&token);
            if pages == 0 && !query.is_empty() {
                req = req.query(query);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let mut v = http::json(req, &format!("Apple Business Manager {} {}", method.to_uppercase(), path)).await?;
            let next = v["links"]["next"].as_str().map(|s| s.to_string());
            strip_links(&mut v);
            pages += 1;
            match merged.as_mut() {
                None => merged = Some(v),
                Some(acc) => {
                    if let (Some(Value::Array(dst)), Some(Value::Array(src))) = (acc.get_mut("data"), v.get("data")) {
                        dst.extend(src.iter().cloned());
                    }
                }
            }
            match next {
                Some(n) if pages < max_pages.max(1) => url = n,
                _ => break,
            }
        }
        Ok(merged.unwrap_or(Value::Null))
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AbmRequestParams {
    /// GET or POST
    pub method: String,
    /// Path, e.g. `orgDevices`, `orgDevices/{id}/assignedServer`, `mdmServers`, `orgDeviceActivities`
    pub path: String,
    /// Query params, e.g. {"limit": 100, "fields[orgDevices]": "serialNumber,deviceModel"}
    pub query: Option<serde_json::Map<String, Value>>,
    /// JSON body
    pub body: Option<Value>,
    /// Follow pagination up to N pages (default 1)
    pub max_pages: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AbmDevicesParams {
    /// Page size (default 100, max 1000)
    pub limit: Option<u32>,
    /// Pages to fetch (default 1)
    pub max_pages: Option<u32>,
    /// Fields to return, e.g. serialNumber,deviceModel,productFamily,status,addedToOrgDateTime
    pub fields: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AbmDeviceParams {
    /// Device id (from abm_devices) — not the serial number
    pub device_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AbmServersParams {
    /// Also list device ids assigned to each MDM server (default false)
    pub include_devices: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AbmAssignParams {
    /// Device ids to (un)assign
    pub device_ids: Vec<String>,
    /// Target MDM server id (from abm_mdm_servers)
    pub mdm_server_id: String,
    /// ASSIGN_DEVICES (default) or UNASSIGN_DEVICES
    pub activity_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AbmActivityParams {
    /// Activity id returned by abm_assign_devices
    pub activity_id: String,
}

#[tool_router(router = abm_router, vis = "pub")]
impl AppleServer {
    /// Generic Apple Business Manager / School Manager API request (organization devices, MDM servers, device activities).
    #[tool(annotations(open_world_hint = true))]
    pub async fn abm_request(&self, Parameters(p): Parameters<AbmRequestParams>) -> ToolResult {
        Ok(render(&self.abm_call(&p.method, &p.path, &query_pairs(&p.query), p.body.as_ref(), p.max_pages.unwrap_or(1).min(50)).await?))
    }

    /// List devices in the organization (serial, model, product family, status, purchase source, enrollment info).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn abm_devices(&self, Parameters(p): Parameters<AbmDevicesParams>) -> ToolResult {
        let mut q = vec![("limit".to_string(), p.limit.unwrap_or(100).clamp(1, 1000).to_string())];
        if let Some(f) = &p.fields { q.push(("fields[orgDevices]".into(), f.clone())); }
        Ok(render(&self.abm_call("GET", "orgDevices", &q, None, p.max_pages.unwrap_or(1).min(50)).await?))
    }

    /// One organization device with its assigned MDM server.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn abm_device(&self, Parameters(p): Parameters<AbmDeviceParams>) -> ToolResult {
        let device = self.abm_call("GET", &format!("orgDevices/{}", p.device_id), &[], None, 1).await?;
        let server = self.abm_call("GET", &format!("orgDevices/{}/assignedServer", p.device_id), &[], None, 1).await.unwrap_or(Value::Null);
        Ok(render(&json!({"device": device["data"], "assigned_server": server["data"]})))
    }

    /// List MDM servers registered in Apple Business/School Manager (optionally with assigned device ids).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn abm_mdm_servers(&self, Parameters(p): Parameters<AbmServersParams>) -> ToolResult {
        let mut servers = self.abm_call("GET", "mdmServers", &[], None, 1).await?;
        if p.include_devices.unwrap_or(false) {
            if let Some(arr) = servers["data"].as_array_mut() {
                for s in arr.iter_mut().take(20) {
                    if let Some(id) = s["id"].as_str().map(|x| x.to_string()) {
                        let devs = self.abm_call("GET", &format!("mdmServers/{id}/relationships/devices"), &[("limit".into(), "1000".into())], None, 5).await.unwrap_or(Value::Null);
                        let ids: Vec<Value> = devs["data"].as_array().cloned().unwrap_or_default().iter().filter_map(|d| d.get("id").cloned()).collect();
                        s["assigned_device_count"] = json!(ids.len());
                        s["assigned_device_ids"] = Value::Array(ids);
                    }
                }
            }
        }
        Ok(render(&servers))
    }

    /// Assign (or unassign) devices to an MDM server. Returns an activity to poll with abm_activity.
    #[tool(annotations(open_world_hint = true))]
    pub async fn abm_assign_devices(&self, Parameters(p): Parameters<AbmAssignParams>) -> ToolResult {
        let devices: Vec<Value> = p.device_ids.iter().map(|id| json!({"type": "orgDevices", "id": id})).collect();
        let body = json!({"data": {"type": "orgDeviceActivities", "attributes": {"activityType": p.activity_type.clone().unwrap_or_else(|| "ASSIGN_DEVICES".into())}, "relationships": {"mdmServer": {"data": {"type": "mdmServers", "id": p.mdm_server_id}}, "devices": {"data": devices}}}});
        Ok(render(&self.abm_call("POST", "orgDeviceActivities", &[], Some(&body), 1).await?))
    }

    /// Status of a device assignment activity (COMPLETED, IN_PROGRESS, FAILED... with download URL of the result log).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn abm_activity(&self, Parameters(p): Parameters<AbmActivityParams>) -> ToolResult {
        Ok(render(&self.abm_call("GET", &format!("orgDeviceActivities/{}", p.activity_id), &[], None, 1).await?))
    }
}
