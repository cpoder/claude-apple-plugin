//! iCloud web session: the private APIs behind icloud.com (what the Python sidecar did with
//! pyicloud), in Rust. Login = Apple's SRP-6a on idmsa.apple.com + two-factor code on a trusted
//! device (or SMS) + `2sv/trust` + `setup.icloud.com/accountLogin`, which hands back the
//! per-account `webservices` map (ckdatabasews, drivews, findme, …). The session (cookies +
//! auth headers) is persisted so the 2FA prompt happens once.
//!
//! Needs `icloud.apple_id` and `icloud.password` (the real Apple ID password, not an
//! app-specific one) in the config. These APIs are undocumented and can change without notice.

pub mod cookies;
pub mod drive;
pub mod findmy;
pub mod photos;
pub mod hme;
pub mod notes;
pub mod reminders;
pub mod srp;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::prelude::*;
use reqwest::header::{HeaderMap, HeaderValue};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::render;
use crate::server::AppleServer;
use cookies::{Cookie, Jar};

const SECTION: &str = "icloud";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.3.1 Safari/605.1.15";
const OAUTH_CLIENT_ID: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
const CLIENT_BUILD_NUMBER: &str = "2534Project66";
const CLIENT_MASTERING_NUMBER: &str = "2534B22";
const IDMSA: &str = "https://idmsa.apple.com";
const AUTH: &str = "https://idmsa.apple.com/appleauth/auth";
const HOME: &str = "https://www.icloud.com";
const SETUP: &str = "https://setup.icloud.com/setup/ws/1";

/// Response headers Apple uses to carry auth state (header → session key).
const HEADER_DATA: &[(&str, &str)] = &[
    ("X-Apple-ID-Account-Country", "account_country"),
    ("X-Apple-ID-Session-Id", "session_id"),
    ("X-Apple-Auth-Attributes", "auth_attributes"),
    ("X-Apple-Session-Token", "session_token"),
    ("X-Apple-TwoSV-Trust-Token", "trust_token"),
    ("X-Apple-TwoSV-Trust-Eligible", "trust_eligible"),
    ("X-Apple-OAuth-Grant-Code", "grant_code"),
    ("X-Apple-I-Rscd", "apple_rscd"),
    ("X-Apple-I-Ercd", "apple_ercd"),
    ("scnt", "scnt"),
];

#[derive(Debug)]
pub enum WebErr {
    /// Apple answered 409 with an HSA2 challenge: a two-factor code is needed.
    TwoFactorRequired(Value),
    /// 421/450/500: the session must be re-established.
    AuthRequired(String),
    Other(String),
}

impl std::fmt::Display for WebErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebErr::TwoFactorRequired(_) => f.write_str("two-factor authentication required: call icloud_2fa with the code shown on your Apple device"),
            WebErr::AuthRequired(s) => write!(f, "iCloud session expired or rejected ({s}); call icloud_login"),
            WebErr::Other(s) => f.write_str(s),
        }
    }
}

impl From<WebErr> for ToolError {
    fn from(e: WebErr) -> Self {
        ToolError::new(e.to_string())
    }
}

impl From<serde_json::Error> for WebErr {
    fn from(e: serde_json::Error) -> Self {
        WebErr::Other(format!("JSON: {e}"))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    client_id: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    cookies: Vec<Cookie>,
    /// Two-factor challenge in progress (survives across processes: the CLI runs one per command).
    #[serde(default)]
    pending: Pending2fa,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Pending2fa {
    /// A challenge was issued by Apple and no code has been accepted yet.
    #[serde(default)]
    active: bool,
    /// Trusted phone number id for the SMS fallback (from the auth page boot args).
    phone_id: Option<Value>,
    phone_non_fteu: Option<bool>,
    phone_label: Option<String>,
    #[serde(default)]
    sms_requested: bool,
}

pub struct WebResp {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl WebResp {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn is_json(&self) -> bool {
        self.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| matches!(ct.split(';').next().unwrap_or("").trim(), "application/json" | "text/json"))
            .unwrap_or(false)
    }
}

pub struct WebSession {
    pub apple_id: String,
    password: String,
    pub http: reqwest::Client,
    jar: Arc<Jar>,
    persisted: Mutex<Persisted>,
    file: PathBuf,
    /// Last `accountLogin` / `validate` response (dsInfo, webservices, hsaTrustedBrowser…).
    login: Mutex<Value>,
    pcs_done: Mutex<Vec<String>>,
}

pub(crate) fn session_dir() -> PathBuf {
    let base = std::env::var("APPLE_MCP_DATA_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty() && !s.contains("${"))
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share")).join("apple-mcp"));
    base.join("icloud-session")
}

fn b64(b: &[u8]) -> String {
    BASE64_STANDARD.encode(b)
}

fn b64d(s: &str) -> Result<Vec<u8>, WebErr> {
    BASE64_STANDARD.decode(s.trim()).map_err(|e| WebErr::Other(format!("base64: {e}")))
}

impl WebSession {
    pub fn new(apple_id: &str, password: &str) -> Result<Arc<Self>, WebErr> {
        let dir = session_dir();
        std::fs::create_dir_all(&dir).map_err(|e| WebErr::Other(format!("cannot create {}: {e}", dir.display())))?;
        crate::setup::restrict(&dir, 0o700);
        let safe: String = apple_id.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
        let file = dir.join(format!("{safe}.json"));
        let mut persisted: Persisted = std::fs::read_to_string(&file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        if persisted.client_id.is_empty() {
            persisted.client_id = uuid::Uuid::new_v4().to_string().to_lowercase();
        }
        let jar = Arc::new(Jar::new(std::mem::take(&mut persisted.cookies)));
        let mut headers = HeaderMap::new();
        headers.insert("Origin", HeaderValue::from_static(HOME));
        headers.insert("Referer", HeaderValue::from_static("https://www.icloud.com/"));
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .cookie_provider(jar.clone())
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| WebErr::Other(e.to_string()))?;
        Ok(Arc::new(Self {
            apple_id: apple_id.to_string(),
            password: password.to_string(),
            http,
            jar,
            persisted: Mutex::new(persisted),
            file,
            login: Mutex::new(Value::Null),
            pcs_done: Mutex::new(Vec::new()),
        }))
    }

    fn save(&self) {
        let mut p = self.persisted.lock().expect("persisted");
        p.cookies = self.jar.snapshot();
        if let Ok(s) = serde_json::to_string_pretty(&*p) {
            let _ = std::fs::write(&self.file, s);
            crate::setup::restrict(&self.file, 0o600);
        }
    }

    fn header_data(&self, key: &str) -> Option<String> {
        self.persisted.lock().expect("persisted").headers.get(key).cloned().filter(|s| !s.is_empty())
    }

    pub fn client_id(&self) -> String {
        self.persisted.lock().expect("persisted").client_id.clone()
    }

    fn pending(&self) -> Pending2fa {
        self.persisted.lock().expect("persisted").pending.clone()
    }

    fn set_pending(&self, p: Pending2fa) {
        self.persisted.lock().expect("persisted").pending = p;
        self.save();
    }

    /// A two-factor challenge is waiting for a code.
    pub fn challenge_pending(&self) -> bool {
        self.pending().active
    }

    /// Value of a session cookie by name (e.g. `X-APPLE-WEBAUTH-VALIDATE`); never log it.
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.jar.get(name)
    }

    pub fn dsid(&self) -> Option<String> {
        let l = self.login.lock().expect("login");
        match &l["dsInfo"]["dsid"] {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    pub fn is_trusted(&self) -> bool {
        self.login.lock().expect("login")["hsaTrustedBrowser"].as_bool().unwrap_or(false)
    }

    pub fn login_data(&self) -> Value {
        self.login.lock().expect("login").clone()
    }

    /// Query params every iCloud web service call carries.
    pub fn params(&self) -> Vec<(String, String)> {
        let mut p = vec![
            ("clientBuildNumber".to_string(), CLIENT_BUILD_NUMBER.to_string()),
            ("clientMasteringNumber".to_string(), CLIENT_MASTERING_NUMBER.to_string()),
            ("clientId".to_string(), self.client_id()),
        ];
        if let Some(d) = self.dsid() {
            p.push(("dsid".into(), d));
        }
        p
    }

    /// Base URL of a webservice from the login map (e.g. `ckdatabasews`, `drivews`, `findme`).
    pub fn ws_url(&self, key: &str) -> Result<String, WebErr> {
        let l = self.login.lock().expect("login");
        match l["webservices"][key]["url"].as_str() {
            Some(u) if u.starts_with("https://") => Ok(u.trim_end_matches('/').to_string()),
            _ => Err(WebErr::Other(format!("iCloud webservice `{key}` not available for this account (log in first with icloud_login)"))),
        }
    }

    fn auth_headers(&self, accept: Option<&str>) -> Vec<(String, String)> {
        let client_id = self.client_id();
        let fd = json!({"U": USER_AGENT, "L": "en-US", "Z": "GMT+00:00", "V": "1.1", "F": ""});
        let mut h = vec![
            ("Accept".to_string(), accept.unwrap_or("application/json, text/javascript").to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Apple-OAuth-Client-Id".to_string(), OAUTH_CLIENT_ID.to_string()),
            ("X-Apple-OAuth-Client-Type".to_string(), "firstPartyAuth".to_string()),
            ("X-Apple-OAuth-Redirect-URI".to_string(), HOME.to_string()),
            ("X-Apple-OAuth-Require-Grant-Code".to_string(), "true".to_string()),
            ("X-Apple-OAuth-Response-Mode".to_string(), "web_message".to_string()),
            ("X-Apple-OAuth-Response-Type".to_string(), "code".to_string()),
            ("X-Apple-OAuth-State".to_string(), client_id.clone()),
            ("X-Apple-Widget-Key".to_string(), OAUTH_CLIENT_ID.to_string()),
            ("X-Apple-FD-Client-Info".to_string(), serde_json::to_string(&fd).unwrap_or_default()),
            ("Referer".to_string(), IDMSA.to_string()),
            ("X-Apple-Frame-Id".to_string(), client_id),
        ];
        for (k, hk) in [("scnt", "scnt"), ("session_id", "X-Apple-ID-Session-Id"), ("auth_attributes", "X-Apple-Auth-Attributes")] {
            if let Some(v) = self.header_data(k) {
                h.push((hk.to_string(), v));
            }
        }
        h
    }

    /// Send a request: capture Apple's auth headers, persist, classify errors like pyicloud does.
    pub async fn send(&self, req: reqwest::RequestBuilder) -> Result<WebResp, WebErr> {
        let resp = req.send().await.map_err(|e| WebErr::Other(format!("iCloud request failed: {e}")))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(|e| WebErr::Other(format!("reading iCloud response failed: {e}")))?.to_vec();
        {
            let mut p = self.persisted.lock().expect("persisted");
            for (h, key) in HEADER_DATA {
                if let Some(v) = headers.get(*h).and_then(|v| v.to_str().ok())
                    && !v.is_empty()
                {
                    p.headers.insert((*key).to_string(), v.to_string());
                }
            }
        }
        self.save();
        let r = WebResp { status, headers, body };
        let status = r.status;
        let ok = (200..300).contains(&status);
        if status == 410 {
            return Err(WebErr::Other("Apple withdrew this endpoint (HTTP 410)".into()));
        }
        let body_json = r.json();
        if let Some(errs) = body_json.as_ref().and_then(|b| b["serviceErrors"].as_array())
            && errs.iter().any(|e| e["code"].as_i64() == Some(-20209) || e["code"].as_str() == Some("-20209"))
        {
            return Err(WebErr::Other("Apple ID account locked (serviceErrors -20209); unlock it at https://iforgot.apple.com".into()));
        }
        if !ok {
            if status == 409
                && let Some(b) = &body_json
                && (b["authType"] == "hsa2" || b["authenticationType"] == "hsa2")
            {
                return Err(WebErr::TwoFactorRequired(b.clone()));
            }
            if status == 450 {
                return Err(WebErr::AuthRequired("HTTP 450".into()));
            }
            if r.is_json() || matches!(status, 409 | 421 | 500) {
                let (code, reason) = body_json.as_ref().map(error_fields).unwrap_or((Value::Null, String::new()));
                return Err(Self::classify(status, code, if reason.is_empty() { r.text() } else { reason }));
            }
            return Err(WebErr::Other(format!("iCloud HTTP {status}: {}", crate::http::clip(r.text().trim(), 500))));
        }
        if r.is_json()
            && let Some(b) = &body_json
            && b.is_object()
        {
            let (code, reason) = error_fields(b);
            if !reason.is_empty() {
                return Err(Self::classify(status, code, reason));
            }
        }
        Ok(r)
    }

    fn classify(status: u16, code: Value, reason: String) -> WebErr {
        match code.as_str() {
            Some("ZONE_NOT_FOUND") | Some("AUTHENTICATION_FAILED") => {
                return WebErr::Other(format!("{reason} ({code}) — log into https://icloud.com/ once to finish setting up this iCloud service"));
            }
            Some("ACCESS_DENIED") => return WebErr::Other(format!("{reason} (ACCESS_DENIED) — Apple is throttling; wait a few minutes")),
            _ => {}
        }
        if matches!(status, 409 | 421 | 450 | 500) || matches!(code.as_i64(), Some(409 | 421 | 450 | 500)) {
            return WebErr::AuthRequired(format!("HTTP {status} {reason}"));
        }
        WebErr::Other(format!("iCloud error{}: {reason}", if code.is_null() { String::new() } else { format!(" {code}") }))
    }

    fn apply(&self, mut req: reqwest::RequestBuilder, params: &[(String, String)], headers: &[(String, String)], body: Option<&Value>) -> reqwest::RequestBuilder {
        if !params.is_empty() {
            req = req.query(params);
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        req
    }

    /// GET a webservice URL with the standard params (+ extra) and return the JSON body.
    pub async fn get(&self, url: &str, extra: &[(&str, &str)]) -> Result<Value, WebErr> {
        let mut params = self.params();
        params.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let r = self.send(self.apply(self.http.get(url), &params, &[], None)).await?;
        Ok(r.json().unwrap_or(Value::Null))
    }

    /// POST JSON to a webservice URL with the standard params (+ extra) and return the JSON body.
    pub async fn post(&self, url: &str, extra: &[(&str, &str)], body: &Value) -> Result<Value, WebErr> {
        let mut params = self.params();
        params.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let r = self.send(self.apply(self.http.post(url), &params, &[], Some(body))).await?;
        Ok(r.json().unwrap_or(Value::Null))
    }

    /// Raw request (any method, optional body, no standard params) for downloads and uploads.
    pub async fn raw(&self, req: reqwest::RequestBuilder) -> Result<WebResp, WebErr> {
        self.send(req).await
    }

    // ------------------------------------------------------------------------------------
    // Login
    // ------------------------------------------------------------------------------------

    /// Log in: cached session first, then the session token, then SRP with the password.
    /// Returns `true` when logged in, `false` when a two-factor code is now required.
    pub async fn login(&self, force: bool) -> Result<bool, WebErr> {
        if force {
            self.jar.clear();
            let mut p = self.persisted.lock().expect("persisted");
            p.headers.clear();
            p.pending = Pending2fa::default();
            drop(p);
            *self.login.lock().expect("login") = Value::Null;
            self.save();
        }
        if self.header_data("session_token").is_some() {
            if let Ok(data) = self.validate().await
                && data["hsaTrustedBrowser"].as_bool().unwrap_or(false)
            {
                *self.login.lock().expect("login") = data;
                return Ok(true);
            }
            match self.account_login().await {
                Ok(()) if self.is_trusted() => return Ok(true),
                Ok(()) | Err(_) => {}
            }
        }
        match self.srp_authentication().await {
            Ok(()) => {}
            Err(WebErr::TwoFactorRequired(_)) => {
                self.trigger_trusted_device_prompt().await;
                return Ok(false);
            }
            Err(e) => return Err(e),
        }
        self.account_login().await?;
        if !self.is_trusted() {
            self.trigger_trusted_device_prompt().await;
            return Ok(false);
        }
        Ok(true)
    }

    async fn validate(&self) -> Result<Value, WebErr> {
        if self.jar.get("X-APPLE-WEBAUTH-TOKEN").is_none() {
            return Err(WebErr::AuthRequired("missing X-APPLE-WEBAUTH-TOKEN cookie".into()));
        }
        let r = self.send(self.http.post(format!("{SETUP}/validate")).body("null")).await?;
        Ok(r.json().unwrap_or(Value::Null))
    }

    async fn account_login(&self) -> Result<(), WebErr> {
        let token = self.header_data("session_token").ok_or_else(|| WebErr::AuthRequired("no session token".into()))?;
        let body = json!({
            "accountCountryCode": self.header_data("account_country"),
            "dsWebAuthToken": token,
            "extended_login": true,
            "trustToken": self.header_data("trust_token").unwrap_or_default(),
        });
        let r = self.send(self.http.post(format!("{SETUP}/accountLogin")).json(&body)).await?;
        let data = r.json().unwrap_or(Value::Null);
        if data["termsUpdateNeeded"].as_bool().unwrap_or(false) {
            return Err(WebErr::Other("Apple requires accepting updated iCloud terms: sign in once at https://www.icloud.com/ and accept them, then retry".into()));
        }
        *self.login.lock().expect("login") = data;
        self.pcs_done.lock().expect("pcs").clear();
        Ok(())
    }

    async fn srp_authentication(&self) -> Result<(), WebErr> {
        let client_id = self.client_id();
        // Prime the auth cookies / scnt like the icloud.com sign-in frame does.
        let r = self
            .http
            .get(format!("{AUTH}/authorize/signin"))
            .query(&[
                ("frame_id", client_id.as_str()),
                ("skVersion", "7"),
                ("iframeid", client_id.as_str()),
                ("client_id", OAUTH_CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", HOME),
                ("response_mode", "web_message"),
                ("state", client_id.as_str()),
                ("authVersion", "latest"),
            ]);
        let _ = self.send(r).await?;

        let srp_client = srp::Client::new(&self.apple_id).map_err(WebErr::Other)?;
        let init = json!({"a": b64(&srp_client.a_bytes()), "accountName": self.apple_id, "protocols": ["s2k", "s2k_fo"]});
        let r = self.send(self.apply(self.http.post(format!("{AUTH}/signin/init")), &[], &self.auth_headers(None), Some(&init))).await.map_err(|e| match e {
            WebErr::Other(s) => WebErr::Other(format!("failed to initiate SRP authentication: {s}")),
            other => other,
        })?;
        let body = r.json().ok_or_else(|| WebErr::Other("signin/init: no JSON body".into()))?;
        let salt = b64d(body["salt"].as_str().unwrap_or(""))?;
        let b = b64d(body["b"].as_str().unwrap_or(""))?;
        let c = body["c"].clone();
        let iterations = body["iteration"].as_u64().unwrap_or(0) as u32;
        let protocol = body["protocol"].as_str().unwrap_or("s2k").to_string();
        let derived = srp::derive_password(&self.password, &salt, iterations, &protocol).map_err(WebErr::Other)?;
        let proof = srp_client.proof(&salt, &b, &derived).map_err(WebErr::Other)?;
        let mut complete = json!({
            "accountName": self.apple_id,
            "c": c,
            "m1": b64(&proof.m1),
            "m2": b64(&proof.m2),
            "rememberMe": true,
            "trustTokens": [],
        });
        if let Some(t) = self.header_data("trust_token") {
            complete["trustTokens"] = json!([t]);
        }
        let req = self.apply(
            self.http.post(format!("{AUTH}/signin/complete")),
            &[("isRememberMeEnabled".into(), "true".into())],
            &self.auth_headers(None),
            Some(&complete),
        );
        match self.send(req).await {
            Ok(_) => Ok(()),
            Err(WebErr::TwoFactorRequired(b)) => Err(WebErr::TwoFactorRequired(b)),
            Err(WebErr::Other(s)) if s.contains("HTTP 401") || s.contains("-20101") || s.to_lowercase().contains("incorrect") => {
                Err(WebErr::Other(format!("Apple rejected the Apple ID / password ({s})")))
            }
            Err(e) => Err(e),
        }
    }

    /// Load the auth page: this is what makes Apple push the 6-digit code to trusted devices,
    /// and its boot args carry the trusted phone number for the SMS fallback.
    async fn trigger_trusted_device_prompt(&self) {
        let req = self.apply(self.http.get(AUTH), &[], &self.auth_headers(Some("text/html")), None);
        let mut pending = Pending2fa { active: true, ..Default::default() };
        if let Ok(r) = self.send(req).await {
            let text = r.text();
            if let Some(boot) = boot_args(&text) {
                let pnv = &boot["direct"]["twoSV"]["bridgeInitiateData"]["phoneNumberVerification"];
                let phone = if pnv["trustedPhoneNumber"].is_object() { pnv["trustedPhoneNumber"].clone() } else { pnv["trustedPhoneNumbers"][0].clone() };
                if phone.is_object() {
                    pending.phone_id = Some(phone["id"].clone());
                    pending.phone_non_fteu = phone["nonFTEU"].as_bool();
                    pending.phone_label = phone["numberWithDialCode"].as_str().or(phone["obfuscatedNumber"].as_str()).map(String::from);
                }
            }
        }
        self.set_pending(pending);
    }

    /// Ask Apple to send the code by SMS to the trusted phone instead.
    pub async fn request_sms(&self) -> Result<String, WebErr> {
        let p = self.pending();
        let (id, non_fteu, label) = (p.phone_id.clone(), p.phone_non_fteu, p.phone_label.clone());
        let id = id.ok_or_else(|| WebErr::Other("no trusted phone number known for this challenge (log in first with icloud_login)".into()))?;
        let mut phone = json!({"id": id});
        if let Some(n) = non_fteu {
            phone["nonFTEU"] = json!(n);
        }
        let body = json!({"phoneNumber": phone, "mode": "sms"});
        let req = self.apply(self.http.put(format!("{AUTH}/verify/phone")), &[], &self.auth_headers(Some("application/json")), Some(&body));
        match self.send(req).await {
            Ok(_) | Err(WebErr::TwoFactorRequired(_)) => {}
            Err(e) => return Err(e),
        }
        self.set_pending(Pending2fa { sms_requested: true, ..p });
        Ok(label.unwrap_or_else(|| "the trusted phone".into()))
    }

    /// Submit the 6-digit code, trust the session, complete the login.
    pub async fn submit_2fa(&self, code: &str) -> Result<(), WebErr> {
        let code = code.trim();
        if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
            return Err(WebErr::Other("the code is 6 digits".into()));
        }
        let p = self.pending();
        if !p.active {
            return Err(WebErr::Other("no two-factor challenge pending: call icloud_login first".into()));
        }
        let (sms, phone_id, non_fteu) = (p.sms_requested, p.phone_id.clone(), p.phone_non_fteu);
        let result = if sms {
            let mut phone = json!({"id": phone_id});
            if let Some(n) = non_fteu {
                phone["nonFTEU"] = json!(n);
            }
            let body = json!({"phoneNumber": phone, "securityCode": {"code": code}, "mode": "sms"});
            let req = self.apply(self.http.post(format!("{AUTH}/verify/phone/securitycode")), &[], &self.auth_headers(Some("application/json, plain/text")), Some(&body));
            self.send(req).await
        } else {
            let body = json!({"securityCode": {"code": code}});
            let req = self.apply(self.http.post(format!("{AUTH}/verify/trusteddevice/securitycode")), &[], &self.auth_headers(Some("application/json")), Some(&body));
            self.send(req).await
        };
        match result {
            Ok(_) => {}
            // Apple may answer 409 while accepting the code; the body's securityCode.valid is the verdict.
            Err(WebErr::TwoFactorRequired(b)) if b["securityCode"]["valid"].as_bool() == Some(true) => {}
            Err(WebErr::TwoFactorRequired(b)) => {
                let msg = b["serviceErrors"][0]["message"].as_str().or(b["securityCode"]["errorMessage"].as_str()).unwrap_or("code rejected");
                return Err(WebErr::Other(format!("Apple rejected the verification code: {msg}")));
            }
            Err(WebErr::Other(s)) => return Err(WebErr::Other(format!("Apple rejected the verification code: {s}"))),
            Err(e) => return Err(e),
        }
        // Trust this session so the next logins skip 2FA, then complete the login.
        let req = self.apply(self.http.get(format!("{AUTH}/2sv/trust")), &[], &self.auth_headers(None), None);
        if let Err(e) = self.send(req).await {
            tracing::warn!("2sv/trust failed: {e}");
        }
        self.set_pending(Pending2fa::default());
        self.account_login().await?;
        if !self.is_trusted() {
            return Err(WebErr::Other("the code was accepted but Apple did not trust the session; run icloud_login again".into()));
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------
    // PCS (Advanced Data Protection): some services need the device to upload cookies first.
    // ------------------------------------------------------------------------------------

    pub async fn ensure_pcs(&self, app_name: &str) -> Result<(), WebErr> {
        if self.pcs_done.lock().expect("pcs").iter().any(|a| a == app_name) {
            return Ok(());
        }
        let params = self.params();
        let state = self.send(self.apply(self.http.post(format!("{SETUP}/requestWebAccessState")), &params, &[], None)).await?.json().unwrap_or(Value::Null);
        if !state["isICDRSDisabled"].as_bool().unwrap_or(false) {
            self.pcs_done.lock().expect("pcs").push(app_name.to_string());
            return Ok(());
        }
        let mut consented = state["isDeviceConsentedForPCS"].as_bool().unwrap_or(true);
        if !consented {
            let resp = self.send(self.apply(self.http.post(format!("{SETUP}/enableDeviceConsentForPCS")), &params, &[], None)).await?.json().unwrap_or(Value::Null);
            if !resp["isDeviceConsentNotificationSent"].as_bool().unwrap_or(false) {
                return Err(WebErr::Other("Unable to request PCS access (Advanced Data Protection): Apple did not send the consent notification".into()));
            }
            for _ in 0..10 {
                if consented {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                let s = self.send(self.apply(self.http.post(format!("{SETUP}/requestWebAccessState")), &params, &[], None)).await?.json().unwrap_or(Value::Null);
                consented = s["isDeviceConsentedForPCS"].as_bool().unwrap_or(true);
            }
        }
        let mut last = String::new();
        for attempt in 0..10 {
            let body = json!({"appName": app_name, "derivedFromUserAction": attempt == 0});
            let resp = self.send(self.apply(self.http.post(format!("{SETUP}/requestPCS")), &params, &[], Some(&body))).await?.json().unwrap_or(Value::Null);
            if resp["status"] == "success" {
                self.pcs_done.lock().expect("pcs").push(app_name.to_string());
                return Ok(());
            }
            last = resp["message"].as_str().unwrap_or("").to_string();
            if last == "Requested the device to upload cookies." || last == "Cookies not available yet on server." {
                tokio::time::sleep(Duration::from_secs(5)).await;
            } else {
                return Err(WebErr::Other(format!("Unable to request PCS access for {app_name}: {last}")));
            }
        }
        Err(WebErr::Other(format!("PCS access for {app_name} not granted in time ({last}); approve the request on one of your Apple devices and retry")))
    }
}

fn error_fields(b: &Value) -> (Value, String) {
    let reason = ["errorMessage", "reason", "errorReason", "error"]
        .iter()
        .find_map(|k| match &b[*k] {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Null => None,
            Value::Bool(false) => None,
            _ => Some("Unknown reason".to_string()),
        })
        .unwrap_or_default();
    let code = if !b["errorCode"].is_null() { b["errorCode"].clone() } else { b["serverErrorCode"].clone() };
    (code, reason)
}

/// JSON inside `<script class="boot_args" …>` of Apple's auth page.
pub fn boot_args(html: &str) -> Option<Value> {
    let idx = html.find("boot_args")?;
    let start = html[idx..].find('>')? + idx + 1;
    let end = html[start..].find("</script>")? + start;
    serde_json::from_str(html[start..end].trim()).ok()
}

// ----------------------------------------------------------------------------------------
// Shared session + tools
// ----------------------------------------------------------------------------------------

static SESSION: OnceLock<tokio::sync::Mutex<Option<Arc<WebSession>>>> = OnceLock::new();

fn slot() -> &'static tokio::sync::Mutex<Option<Arc<WebSession>>> {
    SESSION.get_or_init(|| tokio::sync::Mutex::new(None))
}

impl AppleServer {
    /// The web session for the configured Apple ID (created on first use, not yet logged in).
    pub async fn web_session(&self) -> Result<Arc<WebSession>, ToolError> {
        let cfg = self.cfg();
        let apple_id = cfg.require(SECTION, "apple_id")?;
        let password = cfg.get_str(SECTION, "password").ok_or_else(|| {
            ToolError::new("iCloud web services need `icloud.password` (the full Apple ID password, not an app-specific password) in the config — run /apple:setup, or `apple-mcp setup set --stdin` with {\"icloud\": {\"password\": \"…\"}}")
        })?;
        let mut guard = slot().lock().await;
        if let Some(s) = guard.as_ref()
            && s.apple_id == apple_id
        {
            return Ok(s.clone());
        }
        let s = WebSession::new(&apple_id, &password)?;
        *guard = Some(s.clone());
        Ok(s)
    }

    /// A logged-in web session (logs in silently from the cached session when possible).
    pub async fn web(&self) -> Result<Arc<WebSession>, ToolError> {
        let s = self.web_session().await?;
        if s.is_trusted() {
            return Ok(s);
        }
        match s.login(false).await? {
            true => Ok(s),
            false => Err(ToolError::new("iCloud two-factor authentication required: Apple sent a 6-digit code to your trusted devices; call icloud_2fa with it (or icloud_login with sms=true to receive it by SMS)")),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoginParams {
    /// Discard the cached session and log in from scratch (default false)
    pub force: Option<bool>,
    /// When a two-factor code is pending, ask Apple to send it by SMS to the trusted phone instead of the device prompt
    pub sms: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TwoFactorParams {
    /// The 6-digit code shown on your Apple device (or received by SMS)
    pub code: String,
}

#[tool_router(router = icloud_web_router, vis = "pub")]
impl AppleServer {
    /// Log in to iCloud web services (Drive, Photos, Find My, Reminders, Notes, Hide My Email) with the Apple ID password; the session is cached on disk so the two-factor prompt happens once. Returns whether a 2FA code is now needed.
    #[tool]
    pub async fn icloud_login(&self, Parameters(p): Parameters<LoginParams>) -> ToolResult {
        let s = self.web_session().await?;
        if p.sms.unwrap_or(false) {
            if s.is_trusted() {
                return Ok(render(&json!({"logged_in": true, "note": "already logged in; no SMS sent"})));
            }
            let pending = s.challenge_pending() && !p.force.unwrap_or(false);
            if pending || !s.login(p.force.unwrap_or(false)).await? {
                let to = s.request_sms().await?;
                return Ok(render(&json!({"logged_in": false, "two_factor_required": true, "sms_sent_to": to, "next": "call icloud_2fa with the code received by SMS"})));
            }
        } else if !s.login(p.force.unwrap_or(false)).await? {
            return Ok(render(&json!({"logged_in": false, "two_factor_required": true, "next": "Apple sent a 6-digit code to your trusted Apple devices: call icloud_2fa with it (or icloud_login with sms=true for SMS)"})));
        }
        let d = s.login_data();
        Ok(render(&json!({
            "logged_in": true,
            "apple_id": s.apple_id,
            "name": d["dsInfo"]["fullName"],
            "dsid": s.dsid(),
            "trusted_session": s.is_trusted(),
            "webservices": d["webservices"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
        })))
    }

    /// Submit the 6-digit two-factor code shown on your Apple devices (or received by SMS) and trust this session.
    #[tool]
    pub async fn icloud_2fa(&self, Parameters(p): Parameters<TwoFactorParams>) -> ToolResult {
        let s = self.web_session().await?;
        s.submit_2fa(&p.code).await?;
        let d = s.login_data();
        Ok(render(&json!({"logged_in": true, "trusted_session": s.is_trusted(), "name": d["dsInfo"]["fullName"], "webservices": d["webservices"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default()})))
    }

    /// Account overview from iCloud web: devices registered to the Apple ID, iCloud storage usage, family members.
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_account(&self) -> ToolResult {
        let s = self.web().await?;
        let root = s.ws_url("account")?;
        let devices = s.get(&format!("{root}/setup/web/device/getDevices"), &[]).await.unwrap_or(Value::Null);
        let family = s.get(&format!("{root}/setup/web/family/getFamilyDetails"), &[]).await.unwrap_or(Value::Null);
        let storage = s.post(&format!("{root}/setup/ws/1/storageUsageInfo"), &[], &json!({})).await.unwrap_or(Value::Null);
        let devices: Vec<Value> = devices["devices"]
            .as_array()
            .map(|a| a.iter().map(|d| json!({"name": d["name"], "model": d["modelDisplayName"].as_str().or(d["model"].as_str()), "os": d["osVersion"], "serial": d["serialNumber"]})).collect())
            .unwrap_or_default();
        let family: Vec<Value> = family["familyMembers"].as_array().map(|a| a.iter().map(|m| json!({"name": m["fullName"], "apple_id": m["appleId"]})).collect()).unwrap_or_default();
        let mut by_app = Map::new();
        if let Some(arr) = storage["storageUsageByMedia"].as_array() {
            for u in arr {
                if let Some(k) = u["mediaKey"].as_str() {
                    by_app.insert(k.to_string(), u["usageInBytes"].clone());
                }
            }
        }
        let info = &storage["storageUsageInfo"];
        Ok(render(&json!({
            "devices": devices,
            "storage": {"total_bytes": info["totalStorageInBytes"], "used_bytes": info["usedStorageInBytes"], "by_app": by_app},
            "family": family,
        })))
    }
}

// ----------------------------------------------------------------------------------------
// CLI: `apple-mcp icloud login|2fa <code>|sms|account|lists|reminders <list> [--all]`
// (lets a human or Claude drive the login from a shell when the MCP server is not connected)
// ----------------------------------------------------------------------------------------

pub async fn cli(server: &AppleServer, args: &[String]) -> anyhow::Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let out = match sub {
        "login" => server.icloud_login(Parameters(LoginParams { force: Some(args.iter().any(|a| a == "--force")), sms: Some(args.iter().any(|a| a == "--sms")) })).await,
        "sms" => server.icloud_login(Parameters(LoginParams { force: Some(false), sms: Some(true) })).await,
        "2fa" => {
            let code = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: apple-mcp icloud 2fa <6-digit code>"))?;
            server.icloud_2fa(Parameters(TwoFactorParams { code: code.clone() })).await
        }
        "account" => server.icloud_account().await,
        "lists" => server.icloud_reminders_lists().await,
        "reminders" => {
            let list = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: apple-mcp icloud reminders <list name or id> [--all]"))?;
            server
                .icloud_reminders(Parameters(reminders::RemindersParams { list: list.clone(), include_completed: Some(args.iter().any(|a| a == "--all")), limit: None }))
                .await
        }
        _ => {
            println!("apple-mcp icloud <login [--force] [--sms] | 2fa <code> | sms | account | lists | reminders <list> [--all]>");
            return Ok(());
        }
    };
    match out {
        Ok(s) => println!("{s}"),
        Err(e) => {
            println!("error: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_args_extracted() {
        let html = r#"<html><script type="application/json" class="boot_args">{"direct":{"twoSV":{"bridgeInitiateData":{"phoneNumberVerification":{"trustedPhoneNumbers":[{"id":1,"numberWithDialCode":"+33 •• •• •• •• 12"}]}}}}}</script></html>"#;
        let b = boot_args(html).unwrap();
        assert_eq!(b["direct"]["twoSV"]["bridgeInitiateData"]["phoneNumberVerification"]["trustedPhoneNumbers"][0]["id"], 1);
    }

    #[test]
    fn error_fields_read() {
        assert_eq!(error_fields(&json!({"errorMessage": "bad", "errorCode": -1})), (json!(-1), "bad".into()));
        assert_eq!(error_fields(&json!({"reason": "x", "serverErrorCode": "ZONE_NOT_FOUND"})), (json!("ZONE_NOT_FOUND"), "x".into()));
        assert_eq!(error_fields(&json!({"success": true})).1, "");
        assert_eq!(error_fields(&json!({"error": false})).1, "");
    }
}
