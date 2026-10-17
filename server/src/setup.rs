//! `apple-mcp setup`: automated credential setup.
//!
//! What Apple forces a human to do: click "generate key" in a web console and download the
//! `.p8`/`.pem` once. Everything else is automated here:
//!
//! * **discover** key files in the usual places (config dir, home, Downloads/Desktop/Documents,
//!   the Windows profile under WSL, the current directory), import them into
//!   `~/.config/apple-mcp/keys/` with mode 600 and read the key id from the file name;
//! * **probe** every key against every Apple API it could be enabled for (App Store Connect,
//!   App Store Server, Music, Maps, WeatherKit, APNs, Sign in with Apple, CloudKit, Business
//!   Manager, Search Ads, News, iCloud) and keep what works;
//! * **derive** what can be derived: the Team ID and the apps' bundle ids from App Store
//!   Connect, the Search Ads org id from the ACL list, the issuer id shared by App Store Connect
//!   and App Store Server keys, the Music storefront from the locale;
//! * **generate** the EC P-256 keys CloudKit and Search Ads need and print the public key to
//!   paste in the console;
//! * **report** exactly what is still missing, with the URL to click for each item, as text
//!   for humans or JSON for Claude (`--json`).
//!
//! Subcommands: `auto`, `set`, `discover`, `links`, `open`, `pubkey`, `import`, `wizard`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::prelude::*;
use p256::pkcs8::EncodePublicKey;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::{Config, SERVICES, expand_tilde};
use crate::jwt;

// ---------------------------------------------------------------------------------------------
// Portals: where each credential comes from
// ---------------------------------------------------------------------------------------------

pub struct Portal {
    pub id: &'static str,
    pub url: &'static str,
    pub what: &'static str,
}

pub const PORTALS: &[Portal] = &[
    Portal { id: "membership", url: "https://developer.apple.com/account#MembershipDetailsCard", what: "Team ID (Membership details)" },
    Portal { id: "asc-keys", url: "https://appstoreconnect.apple.com/access/integrations/api", what: "App Store Connect API keys: Generate API Key (role App Manager or Admin), note the Issuer ID, download AuthKey_<KEYID>.p8" },
    Portal { id: "iap-keys", url: "https://appstoreconnect.apple.com/access/integrations/api/subs", what: "In-App Purchase keys (App Store Server API): Generate, download SubscriptionKey_<KEYID>.p8" },
    Portal { id: "dev-keys", url: "https://developer.apple.com/account/resources/authkeys/add", what: "Developer key (one key may enable several services): tick Media Services (MusicKit), WeatherKit, Maps, APNs and/or Sign in with Apple, then download AuthKey_<KEYID>.p8" },
    Portal { id: "services-ids", url: "https://developer.apple.com/account/resources/identifiers/list/serviceId", what: "Services IDs: create one with WeatherKit enabled (its identifier is weatherkit.service_id); for web Sign in with Apple, a Services ID with Sign in with Apple is the client_id" },
    Portal { id: "cloudkit", url: "https://icloud.developer.apple.com/dashboard/", what: "CloudKit Console: container > Settings > Tokens & Keys > Server-to-Server Keys > Add, paste the public key printed by `setup pubkey cloudkit`, copy the Key ID" },
    Portal { id: "abm", url: "https://business.apple.com/", what: "Apple Business Manager: Preferences > API > Add: copy the Client ID (BUSINESSAPI.…) and Key ID, download the .pem (school.apple.com for Apple School Manager)" },
    Portal { id: "searchads", url: "https://app.searchads.apple.com/", what: "Apple Search Ads: Account Settings > API > Create API Client, paste the public key printed by `setup pubkey search_ads`, copy client id, team id and key id" },
    Portal { id: "news", url: "https://www.icloud.com/newspublisher/", what: "News Publisher: channel > API keys: channel ID, key ID, key secret" },
    Portal { id: "icloud-passwords", url: "https://account.apple.com/account/manage/section/security", what: "App-Specific Passwords > Generate (2FA required); apple_id is the Apple ID e-mail (any address)" },
    Portal { id: "vendor", url: "https://appstoreconnect.apple.com/itc/payments_and_financial_reports", what: "Vendor number (top of Payments and Financial Reports), only for sales/finance reports" },
];

pub fn portal(id: &str) -> Option<&'static Portal> {
    PORTALS.iter().find(|p| p.id == id)
}

/// Which portal to send the user to for `section.field`.
fn portal_for(section: &str, field: &str) -> &'static str {
    match (section, field) {
        ("search_ads", _) => "searchads",
        (_, "team_id") => "membership",
        ("app_store_connect", "vendor_number") => "vendor",
        ("app_store_connect", _) => "asc-keys",
        ("app_store_server", "bundle_id") => "asc-keys",
        ("app_store_server", _) => "iap-keys",
        ("weatherkit", "service_id") => "services-ids",
        ("sign_in_with_apple", "client_id") => "services-ids",
        ("music", _) | ("weatherkit", _) | ("maps", _) | ("apns", _) | ("sign_in_with_apple", _) => "dev-keys",
        ("cloudkit", _) => "cloudkit",
        ("business_manager", _) => "abm",
        ("news", _) => "news",
        ("icloud", _) => "icloud-passwords",
        _ => "membership",
    }
}

// ---------------------------------------------------------------------------------------------
// Key files
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyKind {
    /// `AuthKey_<ID>.p8`: App Store Connect API key or developer-portal key (Music/Maps/WeatherKit/APNs/SIWA).
    AuthKey,
    /// `SubscriptionKey_<ID>.p8`: In-App Purchase key (App Store Server API).
    SubscriptionKey,
    /// Any other EC P-256 PEM (CloudKit / Search Ads keys generated locally, Business Manager download).
    Pem,
}

#[derive(Debug, Clone)]
pub struct KeyFile {
    pub path: PathBuf,
    pub pem: String,
    pub kind: KeyKind,
    /// Key id parsed from the file name when it follows Apple's naming.
    pub key_id: Option<String>,
    /// SHA-256 of the public key (SEC1), first 16 hex chars.
    pub fingerprint: String,
    pub imported_from: Option<PathBuf>,
}

impl KeyFile {
    fn label(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

pub fn config_dir() -> PathBuf {
    Config::default_path().parent().map(Path::to_path_buf).unwrap_or_else(|| expand_tilde("~/.config/apple-mcp"))
}

pub fn keys_dir() -> PathBuf {
    config_dir().join("keys")
}

/// Key id from `AuthKey_ABC123DEF4.p8` / `SubscriptionKey_ABC123DEF4.p8` (Apple's download names).
pub fn key_id_from_name(name: &str) -> (KeyKind, Option<String>) {
    let stem = name.strip_suffix(".p8").or_else(|| name.strip_suffix(".pem")).unwrap_or(name);
    for (prefix, kind) in [("AuthKey_", KeyKind::AuthKey), ("SubscriptionKey_", KeyKind::SubscriptionKey)] {
        if let Some(rest) = stem.strip_prefix(prefix) {
            // Browsers add " (1)" / "-1" to duplicate downloads.
            let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if id.len() == 10 {
                return (kind, Some(id.to_uppercase()));
            }
            return (kind, None);
        }
    }
    if name.ends_with(".p8") { (KeyKind::AuthKey, None) } else { (KeyKind::Pem, None) }
}

pub fn fingerprint(pem: &str) -> Option<String> {
    let sk = jwt::signing_key(pem).ok()?;
    let pk = sk.verifying_key().to_sec1_point(false);
    let h = Sha256::digest(pk.as_bytes());
    Some(h.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

fn read_key(path: &Path) -> Option<KeyFile> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 16 * 1024 {
        return None;
    }
    let pem = std::fs::read_to_string(path).ok()?;
    if !pem.contains("PRIVATE KEY") {
        return None;
    }
    let fingerprint = fingerprint(&pem)?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let (kind, key_id) = key_id_from_name(&name);
    Some(KeyFile { path: path.to_path_buf(), pem, kind, key_id, fingerprint, imported_from: None })
}

/// Directories scanned (depth 1) for `.p8` / `.pem` files.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![config_dir(), keys_dir()];
    if let Some(home) = dirs::home_dir() {
        for d in ["", "Downloads", "Desktop", "Documents", "Téléchargements", "Bureau"] {
            dirs.push(if d.is_empty() { home.clone() } else { home.join(d) });
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    // WSL: the Windows user profile(s).
    if let Ok(users) = std::fs::read_dir("/mnt/c/Users") {
        for u in users.flatten() {
            let name = u.file_name().to_string_lossy().into_owned();
            if ["All Users", "Default", "Default User", "Public", "desktop.ini"].contains(&name.as_str()) {
                continue;
            }
            for d in ["Downloads", "Desktop", "Documents", "OneDrive/Desktop", "OneDrive/Documents", "OneDrive/Downloads"] {
                dirs.push(u.path().join(d));
            }
        }
    }
    let mut seen = BTreeSet::new();
    dirs.into_iter().filter(|d| d.is_dir() && seen.insert(d.clone())).collect()
}

/// Find key files everywhere and copy new ones into the keys dir (mode 600). Returns the keys in
/// the keys dir / config dir (one per distinct private key).
pub fn discover(extra_paths: &[PathBuf]) -> anyhow::Result<Vec<KeyFile>> {
    let kdir = keys_dir();
    std::fs::create_dir_all(&kdir)?;
    restrict(&config_dir(), 0o700);
    restrict(&kdir, 0o700);

    let mut candidates: Vec<PathBuf> = extra_paths.to_vec();
    for dir in search_dirs() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_lowercase();
                if name.ends_with(".p8") || name.ends_with(".pem") {
                    candidates.push(p);
                }
            }
        }
    }

    // Keys already in place take precedence; a file elsewhere with the same private key is a duplicate.
    let in_place = |p: &Path| p.starts_with(&kdir) || p.parent() == Some(config_dir().as_path());
    let mut keys: Vec<KeyFile> = Vec::new();
    let mut by_fp: BTreeMap<String, usize> = BTreeMap::new();
    let mut ordered: Vec<PathBuf> = candidates.iter().filter(|p| in_place(p)).cloned().collect();
    ordered.extend(candidates.iter().filter(|p| !in_place(p)).cloned());
    for p in ordered {
        let Some(mut k) = read_key(&p) else { continue };
        if by_fp.contains_key(&k.fingerprint) {
            continue;
        }
        if !in_place(&p) {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "key.p8".into());
            let mut dest = kdir.join(&name);
            let mut n = 1;
            while dest.exists() && read_key(&dest).map(|d| d.fingerprint != k.fingerprint).unwrap_or(true) {
                n += 1;
                dest = kdir.join(format!("{n}-{name}"));
            }
            if !dest.exists() {
                std::fs::write(&dest, &k.pem)?;
            }
            restrict(&dest, 0o600);
            k.imported_from = Some(p.clone());
            k.path = dest;
        } else {
            restrict(&p, 0o600);
        }
        by_fp.insert(k.fingerprint.clone(), keys.len());
        keys.push(k);
    }
    Ok(keys)
}

pub fn restrict(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

/// Generate (once) the EC P-256 key for CloudKit or Search Ads and return (path, public key PEM).
pub fn ensure_generated_key(section: &str) -> anyhow::Result<(PathBuf, String)> {
    let name = match section {
        "cloudkit" => "cloudkit-eckey.pem",
        "search_ads" => "searchads-private-key.pem",
        other => anyhow::bail!("no generated key for section {other}"),
    };
    let kdir = keys_dir();
    std::fs::create_dir_all(&kdir)?;
    restrict(&kdir, 0o700);
    // Reuse a key generated by an earlier version in the config dir itself.
    let legacy = config_dir().join(name);
    let path = if legacy.exists() { legacy } else { kdir.join(name) };
    if !path.exists() {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("system randomness unavailable: {e}"))?;
        let sk = p256::SecretKey::from_slice(&bytes).map_err(|e| anyhow::anyhow!("key generation failed: {e}"))?;
        let pem = sk.to_sec1_pem(p256::pkcs8::LineEnding::LF)?;
        std::fs::write(&path, pem.as_bytes())?;
        restrict(&path, 0o600);
    }
    let pem = std::fs::read_to_string(&path)?;
    let sk = jwt::signing_key(&pem).map_err(|e| anyhow::anyhow!("{e}"))?;
    let public = sk.verifying_key().to_public_key_pem(p256::pkcs8::LineEnding::LF)?;
    Ok((path, public))
}

// ---------------------------------------------------------------------------------------------
// Config file read/merge/write
// ---------------------------------------------------------------------------------------------

pub fn read_config_map() -> anyhow::Result<Map<String, Value>> {
    let path = Config::default_path();
    if !path.exists() {
        return Ok(Map::new());
    }
    let text = std::fs::read_to_string(&path)?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => anyhow::bail!("{}: root must be a JSON object", path.display()),
        Err(e) => anyhow::bail!("{}: invalid JSON ({e}); fix or delete the file", path.display()),
    }
}

pub fn write_config_map(map: &Map<String, Value>) -> anyhow::Result<PathBuf> {
    let path = Config::default_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        restrict(dir, 0o700);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(map.clone()))? + "\n")?;
    restrict(&tmp, 0o600);
    std::fs::rename(&tmp, &path)?;
    restrict(&path, 0o600);
    Ok(path)
}

fn cfg_get<'a>(map: &'a Map<String, Value>, section: &str, key: &str) -> Option<&'a Value> {
    map.get(section)?.as_object()?.get(key)
}

fn cfg_str(map: &Map<String, Value>, section: &str, key: &str) -> Option<String> {
    match cfg_get(map, section, key)? {
        Value::String(s) if !s.trim().is_empty() && !s.starts_with("optional") => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn cfg_set(map: &mut Map<String, Value>, section: &str, key: &str, value: Value) {
    let sec = map.entry(section).or_insert_with(|| Value::Object(Map::new()));
    if !sec.is_object() {
        *sec = Value::Object(Map::new());
    }
    sec.as_object_mut().expect("object").insert(key.to_string(), value);
}

fn top_str(map: &Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Apply `section.key=value` / `key=value` assignments (and JSON objects) to the config map.
/// Values that look like a PEM are stored in a key file instead of inline. An empty value removes the key.
pub fn apply_assignments(map: &mut Map<String, Value>, assignments: &[(String, String)]) -> anyhow::Result<Vec<String>> {
    let mut changed = Vec::new();
    for (k, v) in assignments {
        let (section, key) = match k.split_once('.') {
            Some((s, f)) => (Some(s.trim()), f.trim()),
            None => (None, k.trim()),
        };
        let value: Value = if v.trim().is_empty() {
            Value::Null
        } else if v.contains("PRIVATE KEY") {
            let pem = v.replace("\\n", "\n").trim().to_string() + "\n";
            let section_name = section.unwrap_or("key");
            let key_id = cfg_str(map, section_name, "key_id");
            let path = store_pem(section_name, key_id.as_deref(), &pem)?;
            // private_key / private_key_path both end up as a path.
            let sec = map.entry(section_name).or_insert_with(|| Value::Object(Map::new()));
            if let Some(o) = sec.as_object_mut() {
                o.remove("private_key");
            }
            changed.push(format!("{section_name}.private_key_path"));
            cfg_set(map, section_name, "private_key_path", Value::String(path.display().to_string()));
            continue;
        } else if let Ok(n) = v.trim().parse::<i64>() {
            if key == "org_id" { Value::Number(n.into()) } else { Value::String(v.trim().to_string()) }
        } else if key == "private_key_path" {
            let p = expand_tilde(v.trim());
            if !p.exists() {
                anyhow::bail!("{}: file not found", p.display());
            }
            Value::String(p.display().to_string())
        } else {
            Value::String(v.trim().to_string())
        };
        match section {
            Some(s) => {
                if value.is_null() {
                    if let Some(o) = map.get_mut(s).and_then(|x| x.as_object_mut()) {
                        o.remove(key);
                    }
                } else {
                    cfg_set(map, s, key, value);
                }
                changed.push(format!("{s}.{key}"));
            }
            None => {
                if value.is_null() { map.remove(key); } else { map.insert(key.to_string(), value); }
                changed.push(key.to_string());
            }
        }
    }
    Ok(changed)
}

/// Save a pasted PEM into the keys dir and return its path.
pub fn store_pem(section: &str, key_id: Option<&str>, pem: &str) -> anyhow::Result<PathBuf> {
    jwt::signing_key(pem).map_err(|e| anyhow::anyhow!("the pasted key is not a valid P-256 private key: {e}"))?;
    let kdir = keys_dir();
    std::fs::create_dir_all(&kdir)?;
    restrict(&kdir, 0o700);
    let name = match (section, key_id) {
        ("app_store_server", Some(id)) => format!("SubscriptionKey_{id}.p8"),
        (_, Some(id)) => format!("AuthKey_{id}.p8"),
        ("cloudkit", None) => "cloudkit-eckey.pem".into(),
        ("search_ads", None) => "searchads-private-key.pem".into(),
        (s, None) => format!("{s}.pem"),
    };
    let path = kdir.join(name);
    std::fs::write(&path, pem)?;
    restrict(&path, 0o600);
    Ok(path)
}

/// Merge a JSON object (e.g. from stdin) into the config: `{"team_id": "...", "music": {"key_id": "..."}}`.
pub fn apply_json(map: &mut Map<String, Value>, obj: &Map<String, Value>) -> anyhow::Result<Vec<String>> {
    let mut assignments = Vec::new();
    for (k, v) in obj {
        match v {
            Value::Object(sec) => {
                for (f, fv) in sec {
                    let s = match fv {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    assignments.push((format!("{k}.{f}"), s));
                }
            }
            Value::String(s) => assignments.push((k.clone(), s.clone())),
            Value::Null => assignments.push((k.clone(), String::new())),
            other => assignments.push((k.clone(), other.to_string())),
        }
    }
    apply_assignments(map, &assignments)
}

// ---------------------------------------------------------------------------------------------
// Probes: one cheap authenticated call per service
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Probe {
    Ok(String),
    /// Authentication rejected (key not enabled for this service / wrong ids).
    Denied(String),
    /// Could not conclude (network error, unexpected status).
    Unknown(String),
}

impl Probe {
    fn is_ok(&self) -> bool {
        matches!(self, Probe::Ok(_))
    }
    fn detail(&self) -> &str {
        match self {
            Probe::Ok(s) | Probe::Denied(s) | Probe::Unknown(s) => s,
        }
    }
}

pub struct Prober {
    http: reqwest::Client,
}

fn apple_error(body: &Value) -> String {
    if let Some(errs) = body.get("errors").and_then(|e| e.as_array()) {
        return errs
            .iter()
            .map(|e| {
                format!(
                    "{} {}",
                    e.get("code").and_then(|c| c.as_str()).unwrap_or(""),
                    e.get("detail").or_else(|| e.get("title")).and_then(|c| c.as_str()).unwrap_or("")
                )
                .trim()
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("; ");
    }
    for k in ["errorMessage", "reason", "error_description", "error", "message", "serverErrorCode"] {
        if let Some(s) = body.get(k).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

impl Prober {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("apple-mcp-setup/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(25))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self { http }
    }

    async fn get_json(&self, req: reqwest::RequestBuilder) -> Result<(u16, Value, String), String> {
        let resp = req.send().await.map_err(|e| format!("network: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        Ok((status, body, text))
    }

    fn classify(status: u16, body: &Value, text: &str) -> Probe {
        let detail = {
            let d = apple_error(body);
            if d.is_empty() { text.chars().take(200).collect::<String>() } else { d }
        };
        match status {
            200..=299 => Probe::Ok(format!("HTTP {status}")),
            401 | 403 => Probe::Denied(format!("HTTP {status} {detail}").trim().to_string()),
            _ => Probe::Unknown(format!("HTTP {status} {detail}").trim().to_string()),
        }
    }

    /// App Store Connect: `GET /v1/apps?limit=1`. `issuer=None` means an individual key (`sub: user`).
    pub async fn asc(&self, key: &KeyFile, key_id: &str, issuer: Option<&str>) -> Probe {
        let now = jwt::now();
        let mut claims = json!({"iat": now, "exp": now + 600, "aud": "appstoreconnect-v1"});
        match issuer {
            Some(i) => claims["iss"] = json!(i),
            None => claims["sub"] = json!("user"),
        }
        let tok = match jwt::es256_jwt(&key.pem, json!({"kid": key_id, "typ": "JWT"}), claims) {
            Ok(t) => t,
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        match self.get_json(self.http.get("https://api.appstoreconnect.apple.com/v1/apps?limit=1").bearer_auth(tok)).await {
            Ok((s, b, t)) => Self::classify(s, &b, &t),
            Err(e) => Probe::Unknown(e),
        }
    }

    fn asc_token(key: &KeyFile, key_id: &str, issuer: Option<&str>) -> Option<String> {
        let now = jwt::now();
        let mut claims = json!({"iat": now, "exp": now + 600, "aud": "appstoreconnect-v1"});
        match issuer {
            Some(i) => claims["iss"] = json!(i),
            None => claims["sub"] = json!("user"),
        }
        jwt::es256_jwt(&key.pem, json!({"kid": key_id, "typ": "JWT"}), claims).ok()
    }

    /// Apps (name, bundle id) visible to an App Store Connect key.
    pub async fn asc_apps(&self, key: &KeyFile, key_id: &str, issuer: Option<&str>) -> Vec<(String, String)> {
        let Some(tok) = Self::asc_token(key, key_id, issuer) else { return vec![] };
        let url = "https://api.appstoreconnect.apple.com/v1/apps?limit=200&fields[apps]=name,bundleId";
        match self.get_json(self.http.get(url).bearer_auth(tok)).await {
            Ok((200, body, _)) => body["data"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|app| {
                            Some((
                                app["attributes"]["name"].as_str()?.to_string(),
                                app["attributes"]["bundleId"].as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            _ => vec![],
        }
    }

    /// Team ID = `seedId` of any registered bundle id.
    pub async fn asc_team_id(&self, key: &KeyFile, key_id: &str, issuer: Option<&str>) -> Option<String> {
        let tok = Self::asc_token(key, key_id, issuer)?;
        let url = "https://api.appstoreconnect.apple.com/v1/bundleIds?limit=1&fields[bundleIds]=seedId";
        match self.get_json(self.http.get(url).bearer_auth(tok)).await {
            Ok((200, body, _)) => body["data"][0]["attributes"]["seedId"].as_str().map(|s| s.to_string()),
            _ => None,
        }
    }

    /// App Store Server API: order lookup with an impossible order id → 404 when auth is fine.
    pub async fn storekit(&self, key: &KeyFile, key_id: &str, issuer: &str, bundle_id: &str, sandbox: bool) -> Probe {
        let now = jwt::now();
        let tok = match jwt::es256_jwt(
            &key.pem,
            json!({"kid": key_id, "typ": "JWT"}),
            json!({"iss": issuer, "iat": now, "exp": now + 600, "aud": "appstoreconnect-v1", "bid": bundle_id}),
        ) {
            Ok(t) => t,
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        let host = if sandbox { "https://api.storekit-sandbox.itunes.apple.com" } else { "https://api.storekit.itunes.apple.com" };
        match self.get_json(self.http.get(format!("{host}/inApps/v1/lookup/MQ0000000000")).bearer_auth(tok)).await {
            Ok((200 | 404, _, _)) => Probe::Ok("auth accepted".into()),
            Ok((s, b, t)) => Self::classify(s, &b, &t),
            Err(e) => Probe::Unknown(e),
        }
    }

    pub async fn music(&self, key: &KeyFile, key_id: &str, team: &str) -> Probe {
        let now = jwt::now();
        let tok = match jwt::es256_jwt(&key.pem, json!({"kid": key_id}), json!({"iss": team, "iat": now, "exp": now + 600})) {
            Ok(t) => t,
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        match self.get_json(self.http.get("https://api.music.apple.com/v1/catalog/us/search?term=apple&limit=1").bearer_auth(tok)).await {
            Ok((s, b, t)) => Self::classify(s, &b, &t),
            Err(e) => Probe::Unknown(e),
        }
    }

    pub async fn maps(&self, key: &KeyFile, key_id: &str, team: &str) -> Probe {
        let now = jwt::now();
        let tok = match jwt::es256_jwt(&key.pem, json!({"kid": key_id, "typ": "JWT"}), json!({"iss": team, "iat": now, "exp": now + 600})) {
            Ok(t) => t,
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        match self.get_json(self.http.get("https://maps-api.apple.com/v1/token").bearer_auth(tok)).await {
            Ok((s, b, t)) => Self::classify(s, &b, &t),
            Err(e) => Probe::Unknown(e),
        }
    }

    pub async fn weatherkit(&self, key: &KeyFile, key_id: &str, team: &str, service_id: &str) -> Probe {
        let now = jwt::now();
        let tok = match jwt::es256_jwt(
            &key.pem,
            json!({"kid": key_id, "id": format!("{team}.{service_id}")}),
            json!({"iss": team, "iat": now, "exp": now + 600, "sub": service_id}),
        ) {
            Ok(t) => t,
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        let url = "https://weatherkit.apple.com/api/v1/availability/48.8566/2.3522?country=FR";
        match self.get_json(self.http.get(url).bearer_auth(tok)).await {
            Ok((s, b, t)) => Self::classify(s, &b, &t),
            Err(e) => Probe::Unknown(e),
        }
    }

    /// APNs: a push to an all-zero device token. `InvalidProviderToken` → key not APNs; `BadDeviceToken`
    /// → auth and topic accepted; `TopicDisallowed`/`MissingTopic` → auth accepted, topic to fix.
    pub async fn apns(&self, key: &KeyFile, key_id: &str, team: &str, topic: Option<&str>, sandbox: bool) -> Probe {
        let tok = match jwt::es256_jwt(&key.pem, json!({"kid": key_id}), json!({"iss": team, "iat": jwt::now()})) {
            Ok(t) => t,
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        let host = if sandbox { "https://api.sandbox.push.apple.com" } else { "https://api.push.apple.com" };
        let mut req = self
            .http
            .post(format!("{host}/3/device/{}", "0".repeat(64)))
            .header("authorization", format!("bearer {tok}"))
            .header("apns-push-type", "alert")
            .header("apns-priority", "10")
            .json(&json!({"aps": {"alert": "apple-mcp setup probe"}}));
        if let Some(t) = topic {
            req = req.header("apns-topic", t);
        }
        match self.get_json(req).await {
            Ok((status, body, text)) => {
                let reason = body.get("reason").and_then(|r| r.as_str()).unwrap_or("");
                match reason {
                    "BadDeviceToken" | "DeviceTokenNotForTopic" | "Unregistered" => Probe::Ok(format!("auth and topic accepted ({reason})")),
                    "TopicDisallowed" | "MissingTopic" | "BadTopic" => Probe::Ok(format!("auth accepted, topic needed/mismatch ({reason})")),
                    "InvalidProviderToken" | "ExpiredProviderToken" | "MissingProviderToken" | "Forbidden" => Probe::Denied(format!("HTTP {status} {reason}")),
                    _ => Self::classify(status, &body, &text),
                }
            }
            Err(e) => Probe::Unknown(e),
        }
    }

    /// Sign in with Apple: token request with a bogus code. `invalid_grant` → client secret accepted,
    /// `invalid_client` → wrong key/client_id/team.
    pub async fn siwa(&self, key: &KeyFile, key_id: &str, team: &str, client_id: &str) -> Probe {
        let now = jwt::now();
        let secret = match jwt::es256_jwt(
            &key.pem,
            json!({"kid": key_id}),
            json!({"iss": team, "iat": now, "exp": now + 600, "aud": "https://appleid.apple.com", "sub": client_id}),
        ) {
            Ok(t) => t,
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        let form = [
            ("client_id", client_id.to_string()),
            ("client_secret", secret),
            ("grant_type", "authorization_code".to_string()),
            ("code", "apple-mcp-setup-probe".to_string()),
        ];
        match self.get_json(self.http.post("https://appleid.apple.com/auth/token").form(&form)).await {
            Ok((status, body, text)) => match body.get("error").and_then(|e| e.as_str()) {
                Some("invalid_grant") => Probe::Ok("client secret accepted (invalid_grant on probe code, as expected)".into()),
                Some("invalid_client") => Probe::Denied("invalid_client".into()),
                _ => Self::classify(status, &body, &text),
            },
            Err(e) => Probe::Unknown(e),
        }
    }

    pub async fn cloudkit(&self, key: &KeyFile, key_id: &str, container: &str, environment: &str) -> Probe {
        let path = format!("/database/1/{container}/{environment}/public/zones/list");
        let body = "{}";
        let date = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let body_hash = BASE64_STANDARD.encode(Sha256::digest(body.as_bytes()));
        let message = format!("{date}:{body_hash}:{path}");
        let sig = match jwt::ecdsa_sign_der(&key.pem, message.as_bytes()) {
            Ok(s) => BASE64_STANDARD.encode(s),
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        let req = self
            .http
            .post(format!("https://api.apple-cloudkit.com{path}"))
            .header("Content-Type", "application/json")
            .header("X-Apple-CloudKit-Request-KeyID", key_id)
            .header("X-Apple-CloudKit-Request-ISO8601Date", date)
            .header("X-Apple-CloudKit-Request-SignatureV1", sig)
            .body(body);
        match self.get_json(req).await {
            Ok((status, body, text)) => match body.get("serverErrorCode").and_then(|c| c.as_str()) {
                Some("AUTHENTICATION_FAILED") | Some("AUTHENTICATION_REQUIRED") => Probe::Denied(apple_error(&body)),
                Some(code) => Probe::Unknown(format!("{code} {}", apple_error(&body))),
                None => Self::classify(status, &body, &text),
            },
            Err(e) => Probe::Unknown(e),
        }
    }

    pub async fn abm(&self, key: &KeyFile, key_id: &str, client_id: &str, scope: &str) -> Probe {
        let now = jwt::now();
        let assertion = match jwt::es256_jwt(
            &key.pem,
            json!({"kid": key_id}),
            json!({"sub": client_id, "aud": "https://account.apple.com/auth/oauth2/v2/token", "iat": now, "exp": now + 600, "jti": uuid::Uuid::new_v4().to_string(), "iss": client_id}),
        ) {
            Ok(t) => t,
            Err(e) => return Probe::Unknown(e.to_string()),
        };
        let form = [
            ("grant_type", "client_credentials".to_string()),
            ("client_id", client_id.to_string()),
            ("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_string()),
            ("client_assertion", assertion),
            ("scope", scope.to_string()),
        ];
        match self.get_json(self.http.post("https://account.apple.com/auth/oauth2/token").form(&form)).await {
            Ok((200, _, _)) => Probe::Ok("token issued".into()),
            Ok((400, b, t)) => Probe::Denied(format!("HTTP 400 {}", if apple_error(&b).is_empty() { t } else { apple_error(&b) })),
            Ok((s, b, t)) => Self::classify(s, &b, &t),
            Err(e) => Probe::Unknown(e),
        }
    }

    /// Search Ads: client-credentials token, then the ACL list (org ids). Ok detail carries `orgId:name` pairs.
    pub async fn searchads(&self, key: &KeyFile, key_id: &str, client_id: &str, team: &str) -> (Probe, Vec<(i64, String)>) {
        let now = jwt::now();
        let secret = match jwt::es256_jwt(
            &key.pem,
            json!({"kid": key_id}),
            json!({"sub": client_id, "aud": "https://appleid.apple.com", "iat": now, "exp": now + 600, "iss": team}),
        ) {
            Ok(t) => t,
            Err(e) => return (Probe::Unknown(e.to_string()), vec![]),
        };
        let form = [
            ("grant_type", "client_credentials".to_string()),
            ("client_id", client_id.to_string()),
            ("client_secret", secret),
            ("scope", "searchadsorg".to_string()),
        ];
        let token = match self.get_json(self.http.post("https://appleid.apple.com/auth/oauth2/token").form(&form)).await {
            Ok((200, b, _)) => match b["access_token"].as_str() {
                Some(t) => t.to_string(),
                None => return (Probe::Unknown("no access_token".into()), vec![]),
            },
            Ok((400, b, t)) => return (Probe::Denied(format!("HTTP 400 {}", if apple_error(&b).is_empty() { t } else { apple_error(&b) })), vec![]),
            Ok((s, b, t)) => return (Self::classify(s, &b, &t), vec![]),
            Err(e) => return (Probe::Unknown(e), vec![]),
        };
        match self.get_json(self.http.get("https://api.searchads.apple.com/api/v5/acls").bearer_auth(token)).await {
            Ok((200, b, _)) => {
                let orgs: Vec<(i64, String)> = b["data"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|o| Some((o["orgId"].as_i64()?, o["orgName"].as_str().unwrap_or("").to_string()))).collect())
                    .unwrap_or_default();
                (Probe::Ok(format!("{} organization(s)", orgs.len())), orgs)
            }
            Ok((s, b, t)) => (Self::classify(s, &b, &t), vec![]),
            Err(e) => (Probe::Unknown(e), vec![]),
        }
    }

    pub async fn news(&self, channel_id: &str, key_id: &str, key_secret: &str) -> Probe {
        use hmac::{Hmac, KeyInit, Mac};
        let Ok(secret) = BASE64_STANDARD.decode(key_secret.trim()) else { return Probe::Denied("key_secret is not base64".into()) };
        let url = format!("https://news-api.apple.com/channels/{channel_id}");
        let date = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(&secret) else { return Probe::Denied("key_secret unusable".into()) };
        mac.update(format!("GET{url}{date}").as_bytes());
        let sig = BASE64_STANDARD.encode(mac.finalize().into_bytes());
        let auth = format!("HHMAC; key={key_id}; signature={sig}; date={date}");
        match self.get_json(self.http.get(&url).header("Authorization", auth)).await {
            Ok((s, b, t)) => Self::classify(s, &b, &t),
            Err(e) => Probe::Unknown(e),
        }
    }

    /// iCloud: CalDAV principal lookup with the app-specific password.
    pub async fn icloud(&self, apple_id: &str, password: &str) -> Probe {
        let body = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
        let req = self
            .http
            .request(reqwest::Method::from_bytes(b"PROPFIND").expect("method"), "https://caldav.icloud.com/")
            .basic_auth(apple_id, Some(password))
            .header("Depth", "0")
            .header("Content-Type", "application/xml")
            .body(body);
        match self.get_json(req).await {
            Ok((207 | 200, _, _)) => Probe::Ok("CalDAV principal found".into()),
            Ok((401 | 403, _, _)) => Probe::Denied("iCloud rejected the Apple ID / app-specific password".into()),
            Ok((s, b, t)) => Self::classify(s, &b, &t),
            Err(e) => Probe::Unknown(e),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ServiceReport {
    pub section: String,
    pub label: String,
    /// ok | needs | failed | skipped
    pub status: String,
    /// Fields still needed (`section.field` → how to get it)
    pub needs: Vec<Need>,
    /// Candidate values discovered for a needed field (several apps → choose one)
    pub choices: BTreeMap<String, Vec<String>>,
    pub detail: String,
    pub key_file: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Need {
    pub field: String,
    pub portal: String,
    pub url: String,
    pub how: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Report {
    pub config_path: String,
    pub keys_dir: String,
    pub keys: Vec<Value>,
    pub team_id: Option<String>,
    pub team_id_source: Option<String>,
    pub apps: Vec<Value>,
    pub services: Vec<ServiceReport>,
    pub public_keys: BTreeMap<String, String>,
    pub changed: Vec<String>,
    pub warnings: Vec<String>,
}

fn need(section: &str, field: &str, how: &str) -> Need {
    let pid = portal_for(section, field);
    let p = portal(pid).expect("portal");
    Need { field: format!("{section}.{field}"), portal: pid.into(), url: p.url.into(), how: how.into() }
}

pub struct AutoOptions {
    pub want: Option<BTreeSet<String>>,
    pub assignments: Vec<(String, String)>,
    pub extra_paths: Vec<PathBuf>,
    pub offline: bool,
}

fn wanted(opts: &AutoOptions, section: &str) -> bool {
    opts.want.as_ref().map(|w| w.contains(section)).unwrap_or(true)
}

fn locale_storefront() -> String {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var)
            && let Some(cc) = v.split(['.', '@']).next().and_then(|l| l.split('_').nth(1))
            && cc.len() == 2
        {
            return cc.to_lowercase();
        }
    }
    "us".into()
}

/// Discover, probe, assign, write. The heart of `setup auto`.
pub async fn auto(opts: &AutoOptions) -> anyhow::Result<Report> {
    let mut map = read_config_map()?;
    let mut report = Report { config_path: Config::default_path().display().to_string(), keys_dir: keys_dir().display().to_string(), ..Default::default() };
    report.changed = apply_assignments(&mut map, &opts.assignments)?;
    let before = map.clone();

    // Keys: discovered files + keys referenced by the config (inline or by path).
    let mut keys = discover(&opts.extra_paths)?;
    for spec in SERVICES {
        if let Some(p) = cfg_str(&map, spec.section, "private_key_path") {
            let path = expand_tilde(&p);
            if let Some(k) = read_key(&path)
                && !keys.iter().any(|x| x.fingerprint == k.fingerprint)
            {
                keys.push(k);
            }
        } else if let Some(inline) = cfg_str(&map, spec.section, "private_key") {
            let pem = inline.replace("\\n", "\n");
            if let Some(fp) = fingerprint(&pem)
                && !keys.iter().any(|x| x.fingerprint == fp)
            {
                let key_id = cfg_str(&map, spec.section, "key_id");
                let kind = if spec.section == "app_store_server" { KeyKind::SubscriptionKey } else if key_id.is_some() { KeyKind::AuthKey } else { KeyKind::Pem };
                keys.push(KeyFile { path: PathBuf::from(format!("<inline {}>", spec.section)), pem, kind, key_id, fingerprint: fp, imported_from: None });
            }
        }
    }
    let prober = Prober::new();
    let mut used: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // Helper closures over the config map.
    let get = |map: &Map<String, Value>, s: &str, k: &str| cfg_str(map, s, k);
    let key_for = |map: &Map<String, Value>, keys: &[KeyFile], section: &str| -> Option<KeyFile> {
        let path = get(map, section, "private_key_path").map(|p| expand_tilde(&p));
        let inline = get(map, section, "private_key").map(|s| s.replace("\\n", "\n")).and_then(|p| fingerprint(&p));
        keys.iter().find(|k| path.as_ref().map(|p| &k.path == p).unwrap_or(false) || inline.as_ref().map(|fp| &k.fingerprint == fp).unwrap_or(false)).cloned()
    };
    // Candidate (key, key_id) pairs for a section: the configured key first, then every key of a compatible kind.
    let candidates = |map: &Map<String, Value>, keys: &[KeyFile], section: &str, kinds: &[KeyKind]| -> Vec<(KeyFile, String)> {
        let mut out: Vec<(KeyFile, String)> = Vec::new();
        if let Some(k) = key_for(map, keys, section)
            && let Some(id) = get(map, section, "key_id").or_else(|| k.key_id.clone())
        {
            out.push((k, id));
        }
        for k in keys.iter().filter(|k| kinds.contains(&k.kind)) {
            if let Some(id) = k.key_id.clone()
                && !out.iter().any(|(x, _)| x.fingerprint == k.fingerprint)
            {
                out.push((k.clone(), id));
            }
        }
        // A key with an unknown id can only be used with a configured key_id.
        if out.is_empty()
            && let (Some(k), Some(id)) = (key_for(map, keys, section), get(map, section, "key_id"))
        {
            out.push((k, id));
        }
        out
    };
    let assign = |map: &mut Map<String, Value>, used: &mut BTreeMap<String, Vec<String>>, section: &str, k: &KeyFile, id: &str| {
        if !k.path.starts_with("<inline") {
            cfg_set(map, section, "private_key_path", Value::String(k.path.display().to_string()));
            if let Some(o) = map.get_mut(section).and_then(|x| x.as_object_mut()) {
                o.remove("private_key");
            }
        }
        cfg_set(map, section, "key_id", Value::String(id.to_string()));
        used.entry(k.fingerprint.clone()).or_default().push(section.to_string());
    };

    let mut team_id = top_str(&map, "team_id").or_else(|| std::env::var("APPLE_TEAM_ID").ok().filter(|s| !s.trim().is_empty()));
    let mut team_source = team_id.as_ref().map(|_| "config".to_string());
    let mut issuer = get(&map, "app_store_connect", "issuer_id").or_else(|| get(&map, "app_store_server", "issuer_id"));
    let mut apps: Vec<(String, String)> = Vec::new();
    let mut asc_key: Option<(KeyFile, String, Option<String>)> = None;

    // ---- App Store Connect ---------------------------------------------------------------
    let section = "app_store_connect";
    let label = |s: &str| SERVICES.iter().find(|x| x.section == s).map(|x| x.label.to_string()).unwrap_or_default();
    if wanted(opts, section) {
        let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
        let cands = candidates(&map, &keys, section, &[KeyKind::AuthKey]);
        let mut individual_failed = false;
        if opts.offline {
            sr.status = "skipped".into();
            sr.detail = "offline".into();
        } else {
            'asc: for (k, id) in &cands {
                let mut attempts: Vec<Option<String>> = Vec::new();
                if let Some(i) = &issuer {
                    attempts.push(Some(i.clone()));
                }
                attempts.push(None);
                for iss in attempts {
                    let p = prober.asc(k, id, iss.as_deref()).await;
                    if p.is_ok() {
                        assign(&mut map, &mut used, section, k, id);
                        match &iss {
                            Some(i) => cfg_set(&mut map, section, "issuer_id", Value::String(i.clone())),
                            None => {
                                if let Some(o) = map.get_mut(section).and_then(|x| x.as_object_mut()) {
                                    o.remove("issuer_id");
                                }
                            }
                        }
                        sr.status = "ok".into();
                        sr.detail = format!("{} ({})", p.detail(), if iss.is_some() { "team key" } else { "individual key" });
                        sr.key_file = Some(k.label());
                        asc_key = Some((k.clone(), id.clone(), iss.clone()));
                        break 'asc;
                    } else if iss.is_none() {
                        individual_failed = true;
                        sr.detail = p.detail().to_string();
                    }
                }
            }
        }
        if sr.status == "needs" {
            if cands.is_empty() {
                sr.needs.push(need(section, "private_key_path", "Generate an App Store Connect API key (Team Keys), download AuthKey_<KEYID>.p8 and paste it (or its path) here"));
            } else if issuer.is_none() && individual_failed {
                sr.needs.push(need(section, "issuer_id", "Copy the Issuer ID shown above the keys list (team keys need it; none of the keys worked as an individual key)"));
            } else {
                sr.needs.push(need(section, "private_key_path", &format!("No key was accepted by App Store Connect ({}). Generate a Team Key with role App Manager/Admin and paste it here", sr.detail)));
            }
        }
        report.services.push(sr);
    }

    // Derived from App Store Connect: apps (bundle ids) and the Team ID.
    if let Some((k, id, iss)) = &asc_key {
        apps = prober.asc_apps(k, id, iss.as_deref()).await;
        if team_id.is_none()
            && let Some(t) = prober.asc_team_id(k, id, iss.as_deref()).await
        {
            team_id = Some(t.clone());
            team_source = Some("App Store Connect (bundleIds.seedId)".into());
            map.insert("team_id".into(), Value::String(t));
        }
        if issuer.is_none() {
            issuer = iss.clone();
        }
    }
    report.apps = apps.iter().map(|(n, b)| json!({"name": n, "bundle_id": b})).collect();
    let bundle_ids: Vec<String> = apps.iter().map(|(_, b)| b.clone()).collect();
    let single_bundle = if bundle_ids.len() == 1 { Some(bundle_ids[0].clone()) } else { None };

    // ---- App Store Server (StoreKit) -------------------------------------------------------
    let section = "app_store_server";
    if wanted(opts, section) {
        let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
        let cands = candidates(&map, &keys, section, &[KeyKind::SubscriptionKey]);
        let bundle = get(&map, section, "bundle_id").or_else(|| single_bundle.clone());
        let sandbox = get(&map, section, "environment").map(|e| e.starts_with("sandbox")).unwrap_or(false);
        if opts.offline {
            sr.status = "skipped".into();
        } else if let (Some(iss), false) = (&issuer, cands.is_empty()) {
            let bundles: Vec<String> = match &bundle {
                Some(b) => vec![b.clone()],
                None => bundle_ids.clone(),
            };
            'sk: for (k, id) in &cands {
                for b in &bundles {
                    let p = prober.storekit(k, id, iss, b, sandbox).await;
                    if p.is_ok() {
                        assign(&mut map, &mut used, section, k, id);
                        cfg_set(&mut map, section, "issuer_id", Value::String(iss.clone()));
                        cfg_set(&mut map, section, "bundle_id", Value::String(b.clone()));
                        if get(&map, section, "environment").is_none() {
                            cfg_set(&mut map, section, "environment", Value::String("production".into()));
                        }
                        sr.status = "ok".into();
                        sr.detail = format!("{} (bundle {b})", p.detail());
                        sr.key_file = Some(k.label());
                        break 'sk;
                    }
                    sr.detail = p.detail().to_string();
                }
            }
        }
        if sr.status == "needs" {
            if cands.is_empty() {
                sr.needs.push(need(section, "private_key_path", "Generate an In-App Purchase key, download SubscriptionKey_<KEYID>.p8 and paste it (or its path) here"));
            }
            if issuer.is_none() {
                sr.needs.push(need(section, "issuer_id", "Issuer ID shown on the In-App Purchase keys page (same as the App Store Connect API issuer)"));
            }
            if bundle.is_none() && bundle_ids.is_empty() {
                sr.needs.push(need(section, "bundle_id", "Bundle id of the app whose purchases you will query"));
            } else if bundle.is_none() {
                sr.needs.push(need(section, "bundle_id", "Which app? (several apps found in App Store Connect)"));
                sr.choices.insert(format!("{section}.bundle_id"), bundle_ids.clone());
            }
            if sr.needs.is_empty() {
                sr.needs.push(need(section, "private_key_path", &format!("The In-App Purchase key was rejected ({}); generate a new one and paste it here", sr.detail)));
            }
        }
        report.services.push(sr);
    }

    // ---- Team-ID services: Music, Maps, WeatherKit, APNs, Sign in with Apple ----------------
    let team_needed: Vec<&str> = ["music", "maps", "weatherkit", "apns", "sign_in_with_apple"].into_iter().filter(|s| wanted(opts, s)).collect();
    if !team_needed.is_empty() && team_id.is_none() {
        for (i, s) in team_needed.iter().enumerate() {
            let mut sr = ServiceReport { section: s.to_string(), label: label(s), status: "needs".into(), detail: "waiting for team_id".into(), ..Default::default() };
            if i == 0 {
                let mut n = need(s, "team_id", "Team ID (10 characters) from Membership details, shared by Music, Maps, WeatherKit, APNs and Sign in with Apple; derived automatically once App Store Connect is configured");
                n.field = "team_id".into();
                sr.needs.push(n);
            }
            report.services.push(sr);
        }
    } else if let Some(team) = team_id.clone() {
        let dev_keys = |map: &Map<String, Value>, keys: &[KeyFile], s: &str| candidates(map, keys, s, &[KeyKind::AuthKey]);

        if wanted(opts, "music") {
            let section = "music";
            let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
            let cands = dev_keys(&map, &keys, section);
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                for (k, id) in &cands {
                    let p = prober.music(k, id, &team).await;
                    if p.is_ok() {
                        assign(&mut map, &mut used, section, k, id);
                        if get(&map, section, "storefront").is_none() {
                            cfg_set(&mut map, section, "storefront", Value::String(locale_storefront()));
                        }
                        sr.status = "ok".into();
                        sr.detail = p.detail().to_string();
                        sr.key_file = Some(k.label());
                        break;
                    }
                    sr.detail = p.detail().to_string();
                }
            }
            if sr.status == "needs" {
                sr.needs.push(need(section, "private_key_path", if cands.is_empty() { "Create a key with Media Services (MusicKit) enabled and paste AuthKey_<KEYID>.p8 here" } else { "None of the keys has MusicKit enabled; create one (Media Services) and paste it here" }));
            }
            report.services.push(sr);
        }

        if wanted(opts, "maps") {
            let section = "maps";
            let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
            let cands = dev_keys(&map, &keys, section);
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                for (k, id) in &cands {
                    let p = prober.maps(k, id, &team).await;
                    if p.is_ok() {
                        assign(&mut map, &mut used, section, k, id);
                        sr.status = "ok".into();
                        sr.detail = p.detail().to_string();
                        sr.key_file = Some(k.label());
                        break;
                    }
                    sr.detail = p.detail().to_string();
                }
            }
            if sr.status == "needs" {
                sr.needs.push(need(section, "private_key_path", if cands.is_empty() { "Create a key with Maps enabled and paste AuthKey_<KEYID>.p8 here" } else { "None of the keys has Maps enabled; create one and paste it here" }));
            }
            report.services.push(sr);
        }

        if wanted(opts, "weatherkit") {
            let section = "weatherkit";
            let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
            let cands = dev_keys(&map, &keys, section);
            match get(&map, section, "service_id") {
                None => {
                    sr.needs.push(need(section, "service_id", "Create a Services ID with WeatherKit enabled (e.g. com.yourcompany.weatherkit) and give its identifier"));
                    if cands.is_empty() {
                        sr.needs.push(need(section, "private_key_path", "Create a key with WeatherKit enabled and paste AuthKey_<KEYID>.p8 here"));
                    }
                }
                Some(service_id) => {
                    if opts.offline {
                        sr.status = "skipped".into();
                    } else {
                        for (k, id) in &cands {
                            let p = prober.weatherkit(k, id, &team, &service_id).await;
                            if p.is_ok() {
                                assign(&mut map, &mut used, section, k, id);
                                sr.status = "ok".into();
                                sr.detail = p.detail().to_string();
                                sr.key_file = Some(k.label());
                                break;
                            }
                            sr.detail = p.detail().to_string();
                        }
                    }
                    if sr.status == "needs" {
                        sr.needs.push(need(section, "private_key_path", &format!("No key accepted by WeatherKit with service id {service_id} ({}). Check the key has WeatherKit enabled, the Services ID has WeatherKit, and wait ~30 min after enabling", sr.detail)));
                    }
                }
            }
            report.services.push(sr);
        }

        if wanted(opts, "apns") {
            let section = "apns";
            let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
            let cands = dev_keys(&map, &keys, section);
            let topic = get(&map, section, "topic").or_else(|| single_bundle.clone());
            let sandbox = get(&map, section, "environment").map(|e| e.starts_with("sandbox") || e.starts_with("dev")).unwrap_or(false);
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                for (k, id) in &cands {
                    let p = prober.apns(k, id, &team, topic.as_deref(), sandbox).await;
                    if p.is_ok() {
                        assign(&mut map, &mut used, section, k, id);
                        if let Some(t) = &topic {
                            cfg_set(&mut map, section, "topic", Value::String(t.clone()));
                        }
                        if get(&map, section, "environment").is_none() {
                            cfg_set(&mut map, section, "environment", Value::String("production".into()));
                        }
                        sr.status = if topic.is_some() { "ok".into() } else { "needs".into() };
                        sr.detail = p.detail().to_string();
                        sr.key_file = Some(k.label());
                        break;
                    }
                    sr.detail = p.detail().to_string();
                }
            }
            if sr.status == "needs" {
                if sr.key_file.is_none() {
                    sr.needs.push(need(section, "private_key_path", if cands.is_empty() { "Create a key with APNs enabled and paste AuthKey_<KEYID>.p8 here" } else { "None of the keys has APNs enabled; create one and paste it here" }));
                }
                if topic.is_none() {
                    sr.needs.push(need(section, "topic", "Bundle id of the app to push to"));
                    if !bundle_ids.is_empty() {
                        sr.choices.insert(format!("{section}.topic"), bundle_ids.clone());
                    }
                }
            }
            report.services.push(sr);
        }

        if wanted(opts, "sign_in_with_apple") {
            let section = "sign_in_with_apple";
            let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
            let cands = dev_keys(&map, &keys, section);
            let client_ids: Vec<String> = match get(&map, section, "client_id") {
                Some(c) => vec![c],
                None => bundle_ids.clone(),
            };
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                'siwa: for (k, id) in &cands {
                    for c in &client_ids {
                        let p = prober.siwa(k, id, &team, c).await;
                        if p.is_ok() {
                            assign(&mut map, &mut used, section, k, id);
                            cfg_set(&mut map, section, "client_id", Value::String(c.clone()));
                            sr.status = "ok".into();
                            sr.detail = format!("{} (client_id {c})", p.detail());
                            sr.key_file = Some(k.label());
                            break 'siwa;
                        }
                        sr.detail = p.detail().to_string();
                    }
                }
            }
            if sr.status == "needs" {
                if cands.is_empty() {
                    sr.needs.push(need(section, "private_key_path", "Create a key with Sign in with Apple enabled (choose the primary App ID) and paste AuthKey_<KEYID>.p8 here"));
                }
                if get(&map, section, "client_id").is_none() {
                    sr.needs.push(need(section, "client_id", "Services ID (web) or app bundle id (native) used as client_id"));
                    if !bundle_ids.is_empty() {
                        sr.choices.insert(format!("{section}.client_id"), bundle_ids.clone());
                    }
                }
                if sr.needs.is_empty() {
                    sr.needs.push(need(section, "private_key_path", &format!("Apple rejected the client secret ({}). Check the key has Sign in with Apple enabled for this App ID", sr.detail)));
                }
            }
            report.services.push(sr);
        }
    }

    // ---- CloudKit ------------------------------------------------------------------------
    let section = "cloudkit";
    if wanted(opts, section) {
        let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
        let (gen_path, public) = ensure_generated_key(section)?;
        report.public_keys.insert(section.into(), public);
        let key = key_for(&map, &keys, section).or_else(|| read_key(&gen_path));
        let key_id = get(&map, section, "key_id");
        let containers: Vec<String> = match get(&map, section, "container") {
            Some(c) => vec![c],
            None => bundle_ids.iter().map(|b| format!("iCloud.{b}")).collect(),
        };
        let env = get(&map, section, "environment").unwrap_or_else(|| "development".into());
        if let (Some(k), Some(id), false) = (&key, &key_id, containers.is_empty()) {
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                for c in &containers {
                    let p = prober.cloudkit(k, id, c, &env).await;
                    if p.is_ok() {
                        assign(&mut map, &mut used, section, k, id);
                        cfg_set(&mut map, section, "container", Value::String(c.clone()));
                        cfg_set(&mut map, section, "environment", Value::String(env.clone()));
                        sr.status = "ok".into();
                        sr.detail = format!("{} (container {c}, {env})", p.detail());
                        sr.key_file = Some(k.label());
                        break;
                    }
                    sr.detail = p.detail().to_string();
                }
            }
        }
        if sr.status == "needs" {
            if key_id.is_none() {
                sr.needs.push(need(section, "key_id", "Add a Server-to-Server key in CloudKit Console with the public key printed by this setup (public_keys.cloudkit), then give the Key ID"));
            }
            if get(&map, section, "container").is_none() {
                sr.needs.push(need(section, "container", "Container identifier, e.g. iCloud.com.yourcompany.app"));
                if !bundle_ids.is_empty() {
                    sr.choices.insert(format!("{section}.container"), containers.clone());
                }
            }
            if sr.needs.is_empty() {
                sr.needs.push(need(section, "key_id", &format!("CloudKit rejected the key ({}). Check the Key ID matches the public key printed by this setup and the container/environment", sr.detail)));
            }
        }
        report.services.push(sr);
    }

    // ---- Business Manager ----------------------------------------------------------------
    let section = "business_manager";
    if wanted(opts, section) {
        let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
        let client_id = get(&map, section, "client_id");
        let key_id = get(&map, section, "key_id");
        let scope = get(&map, section, "scope").unwrap_or_else(|| "business.api".into());
        if let (Some(c), Some(id)) = (&client_id, &key_id) {
            let mut cands: Vec<KeyFile> = key_for(&map, &keys, section).into_iter().collect();
            for k in keys.iter().filter(|k| k.kind == KeyKind::Pem).chain(keys.iter()) {
                if !cands.iter().any(|x| x.fingerprint == k.fingerprint) {
                    cands.push(k.clone());
                }
            }
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                for k in &cands {
                    let p = prober.abm(k, id, c, &scope).await;
                    if p.is_ok() {
                        assign(&mut map, &mut used, section, k, id);
                        cfg_set(&mut map, section, "scope", Value::String(scope.clone()));
                        sr.status = "ok".into();
                        sr.detail = p.detail().to_string();
                        sr.key_file = Some(k.label());
                        break;
                    }
                    sr.detail = p.detail().to_string();
                }
            }
        }
        if sr.status == "needs" {
            if client_id.is_none() {
                sr.needs.push(need(section, "client_id", "Client ID (BUSINESSAPI.…) from Preferences > API"));
            }
            if key_id.is_none() {
                sr.needs.push(need(section, "key_id", "Key ID shown next to the API client"));
            }
            if sr.needs.is_empty() {
                sr.needs.push(need(section, "private_key_path", &format!("No key file was accepted ({}). Paste the .pem downloaded from Business Manager here", sr.detail)));
            }
        }
        report.services.push(sr);
    }

    // ---- Search Ads ----------------------------------------------------------------------
    let section = "search_ads";
    if wanted(opts, section) {
        let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
        let (gen_path, public) = ensure_generated_key(section)?;
        report.public_keys.insert(section.into(), public);
        let key = key_for(&map, &keys, section).or_else(|| read_key(&gen_path));
        let client_id = get(&map, section, "client_id");
        let sa_team = get(&map, section, "team_id");
        let key_id = get(&map, section, "key_id");
        if let (Some(k), Some(c), Some(t), Some(id)) = (&key, &client_id, &sa_team, &key_id) {
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                let (p, orgs) = prober.searchads(k, id, c, t).await;
                if p.is_ok() {
                    assign(&mut map, &mut used, section, k, id);
                    let org = get(&map, section, "org_id");
                    if org.is_none() && orgs.len() == 1 {
                        cfg_set(&mut map, section, "org_id", Value::Number(orgs[0].0.into()));
                    }
                    if org.is_some() || orgs.len() == 1 {
                        sr.status = "ok".into();
                    } else {
                        sr.needs.push(need(section, "org_id", "Several organizations: which one?"));
                        sr.choices.insert(format!("{section}.org_id"), orgs.iter().map(|(i, n)| format!("{i} ({n})")).collect());
                    }
                    sr.detail = p.detail().to_string();
                    sr.key_file = Some(k.label());
                } else {
                    sr.detail = p.detail().to_string();
                }
            }
        }
        if sr.status == "needs" && sr.needs.is_empty() {
            if client_id.is_none() {
                sr.needs.push(need(section, "client_id", "Client ID (SEARCHADS.…) shown after creating the API client with the public key printed by this setup (public_keys.search_ads)"));
            }
            if sa_team.is_none() {
                sr.needs.push(need(section, "team_id", "Team ID (SEARCHADS.…) shown on the same page"));
            }
            if key_id.is_none() {
                sr.needs.push(need(section, "key_id", "Key ID shown on the same page"));
            }
            if sr.needs.is_empty() {
                sr.needs.push(need(section, "key_id", &format!("Search Ads rejected the credentials ({}). Re-check client id / team id / key id against the API page", sr.detail)));
            }
        }
        report.services.push(sr);
    }

    // ---- News ----------------------------------------------------------------------------
    let section = "news";
    if wanted(opts, section) {
        let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
        let (ch, id, secret) = (get(&map, section, "channel_id"), get(&map, section, "key_id"), get(&map, section, "key_secret"));
        if let (Some(ch), Some(id), Some(secret)) = (&ch, &id, &secret) {
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                let p = prober.news(ch, id, secret).await;
                sr.detail = p.detail().to_string();
                if p.is_ok() {
                    sr.status = "ok".into();
                }
            }
        }
        if sr.status == "needs" {
            for (f, how) in [("channel_id", "Channel ID"), ("key_id", "API key ID"), ("key_secret", "API key secret")] {
                if get(&map, section, f).is_none() {
                    sr.needs.push(need(section, f, &format!("{how} from News Publisher > channel > API keys")));
                }
            }
            if sr.needs.is_empty() {
                sr.needs.push(need(section, "key_secret", &format!("Apple News rejected the credentials ({})", sr.detail)));
            }
        }
        report.services.push(sr);
    }

    // ---- iCloud --------------------------------------------------------------------------
    let section = "icloud";
    if wanted(opts, section) {
        let mut sr = ServiceReport { section: section.into(), label: label(section), status: "needs".into(), ..Default::default() };
        let (id, pw) = (get(&map, section, "apple_id"), get(&map, section, "app_specific_password"));
        if let (Some(id), Some(pw)) = (&id, &pw) {
            if opts.offline {
                sr.status = "skipped".into();
            } else {
                let p = prober.icloud(id, pw).await;
                sr.detail = p.detail().to_string();
                if p.is_ok() {
                    sr.status = "ok".into();
                }
            }
        }
        if sr.status == "needs" {
            if id.is_none() {
                sr.needs.push(need(section, "apple_id", "iCloud e-mail address"));
            }
            if pw.is_none() {
                sr.needs.push(need(section, "app_specific_password", "Generate an app-specific password (xxxx-xxxx-xxxx-xxxx) and paste it here"));
            }
            if sr.needs.is_empty() {
                sr.needs.push(need(section, "app_specific_password", &format!("iCloud rejected the credentials ({}). Check in order: iCloud Calendars and Contacts are turned on for this Apple ID (Settings > [your name] > iCloud on an iPhone or Mac, or visible on icloud.com); the app-specific password is typed with its dashes (xxxx-xxxx-xxxx-xxxx) and was generated after the last Apple ID password change (a password change revokes all of them); apple_id is the exact Apple ID e-mail (a Gmail or other address works, no @icloud.com alias needed)", sr.detail)));
            }
        }
        report.services.push(sr);
    }

    // ---- Write ---------------------------------------------------------------------------
    if map != before || !Config::default_path().exists() {
        let path = write_config_map(&map)?;
        for (k, v) in &map {
            if before.get(k) != Some(v) && !report.changed.contains(k) {
                report.changed.push(k.clone());
            }
        }
        report.config_path = path.display().to_string();
    }
    report.team_id = team_id;
    report.team_id_source = team_source;
    report.keys = keys
        .iter()
        .map(|k| {
            json!({
                "file": k.path.display().to_string(),
                "kind": format!("{:?}", k.kind),
                "key_id": k.key_id,
                "fingerprint": k.fingerprint,
                "imported_from": k.imported_from.as_ref().map(|p| p.display().to_string()),
                "used_by": used.get(&k.fingerprint).cloned().unwrap_or_default(),
            })
        })
        .collect();
    for k in &keys {
        if let Some(src) = &k.imported_from {
            report.warnings.push(format!("{} was copied to {}; delete the original once everything works", src.display(), k.path.display()));
        }
    }
    Ok(report)
}

/// Human-readable rendering of the report.
pub fn render_report(r: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!("config: {}\nkeys:   {}\n", r.config_path, r.keys_dir));
    if let Some(t) = &r.team_id {
        out.push_str(&format!("team:   {t} ({})\n", r.team_id_source.clone().unwrap_or_default()));
    }
    if !r.apps.is_empty() {
        out.push_str("apps:   ");
        out.push_str(&r.apps.iter().map(|a| format!("{} ({})", a["name"].as_str().unwrap_or(""), a["bundle_id"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", "));
        out.push('\n');
    }
    if r.keys.is_empty() {
        out.push_str("\nNo key file found. Download one from Apple and drop it in ~/Downloads (or paste it): it will be picked up.\n");
    } else {
        out.push_str("\nKey files:\n");
        for k in &r.keys {
            let used: Vec<String> = k["used_by"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
            out.push_str(&format!(
                "  {:<40} {:<12} {}\n",
                Path::new(k["file"].as_str().unwrap_or("")).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                k["key_id"].as_str().unwrap_or("(id?)"),
                if used.is_empty() { "not used by any service yet".to_string() } else { format!("→ {}", used.join(", ")) }
            ));
        }
    }
    out.push_str("\nServices:\n");
    for s in &r.services {
        let mark = match s.status.as_str() {
            "ok" => "[ok]     ",
            "skipped" => "[skipped]",
            _ => "[needs]  ",
        };
        out.push_str(&format!("  {mark} {:<20} {}\n", s.section, if s.status == "ok" { s.detail.clone() } else { s.label.clone() }));
    }
    let todo: Vec<&ServiceReport> = r.services.iter().filter(|s| !s.needs.is_empty()).collect();
    if !todo.is_empty() {
        out.push_str("\nTo finish, give these (apple-mcp setup auto section.key=value …, or paste the key file content):\n");
        for s in todo {
            out.push_str(&format!("\n  {} — {}\n", s.section, s.label));
            for n in &s.needs {
                out.push_str(&format!("    {}: {}\n      → {}\n", n.field, n.how, n.url));
                if let Some(ch) = s.choices.get(&n.field) {
                    out.push_str(&format!("      choices: {}\n", ch.join(" | ")));
                }
            }
        }
    }
    for (svc, pk) in &r.public_keys {
        if r.services.iter().any(|s| s.section == *svc && s.status != "ok") {
            out.push_str(&format!("\nPublic key to paste for {svc}:\n{pk}"));
        }
    }
    for w in &r.warnings {
        out.push_str(&format!("\nnote: {w}\n"));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Browser
// ---------------------------------------------------------------------------------------------

/// Open a URL in the user's browser (Linux, macOS, WSL → Windows browser). Returns what was tried.
pub fn open_url(url: &str) -> Result<String, String> {
    let mut attempts: Vec<(&str, Vec<String>)> = Vec::new();
    if cfg!(target_os = "macos") {
        attempts.push(("open", vec![url.into()]));
    }
    if Path::new("/mnt/c/Windows").exists() || Path::new("/mnt/c/windows").exists() {
        attempts.push(("rundll32.exe", vec!["url.dll,FileProtocolHandler".into(), url.into()]));
        attempts.push(("powershell.exe", vec!["-NoProfile".into(), "-Command".into(), format!("Start-Process '{url}'")]));
    }
    attempts.push(("xdg-open", vec![url.into()]));
    attempts.push(("wslview", vec![url.into()]));
    for (cmd, args) in attempts {
        if let Ok(st) = std::process::Command::new(cmd).args(&args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()
            && st.success()
        {
            return Ok(cmd.to_string());
        }
    }
    Err("no browser opener found (open / xdg-open / wslview / rundll32.exe)".into())
}

// ---------------------------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------------------------

const USAGE: &str = "apple-mcp setup — automated credential setup

  setup                      wizard in a terminal (auto + prompts); `auto` when not a TTY
  setup auto [opts] [k=v…]   discover key files, probe every key against every Apple API,
                             derive team id / bundle ids, write the config, list what is missing
      --json                 machine-readable report
      --want a,b             only these sections (app_store_connect, app_store_server, music,
                             weatherkit, maps, cloudkit, apns, sign_in_with_apple,
                             business_manager, search_ads, news, icloud)
      --key <path>           also consider this key file (repeatable)
      --offline              no network: discovery and config only
      k=v / section.k=v      set values first (a PEM value is stored as a key file)
      --stdin                read a JSON object of values from stdin and merge it
  setup set k=v… | --stdin   write values without probing
  setup discover [--json]    list / import key files only
  setup links [id]           URLs to click for each credential
  setup open <id>            open that URL in the browser (asc-keys, dev-keys, services-ids,
                             iap-keys, cloudkit, abm, searchads, news, icloud-passwords, membership, vendor)
  setup pubkey cloudkit|search_ads   generate (once) the EC key and print the public key to paste
  setup import <path>…       copy key files into the keys dir (mode 600)
";

fn parse_kv(s: &str) -> Option<(String, String)> {
    let (k, v) = s.split_once('=')?;
    if k.trim().is_empty() || k.contains(char::is_whitespace) {
        return None;
    }
    Some((k.trim().to_string(), v.to_string()))
}

fn read_stdin_json() -> anyhow::Result<Map<String, Value>> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    match serde_json::from_str::<Value>(&s)? {
        Value::Object(m) => Ok(m),
        _ => anyhow::bail!("stdin must be a JSON object"),
    }
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "" => {
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                wizard().await
            } else {
                run_auto(&[]).await
            }
        }
        "wizard" => wizard().await,
        "auto" => run_auto(&args[1..]).await,
        "set" => {
            let mut map = read_config_map()?;
            let mut changed = Vec::new();
            let mut kv = Vec::new();
            for a in &args[1..] {
                if a == "--stdin" {
                    changed.extend(apply_json(&mut map, &read_stdin_json()?)?);
                } else if let Some(p) = parse_kv(a) {
                    kv.push(p);
                } else {
                    anyhow::bail!("expected section.key=value, got `{a}`");
                }
            }
            changed.extend(apply_assignments(&mut map, &kv)?);
            let path = write_config_map(&map)?;
            println!("updated {} ({})", path.display(), if changed.is_empty() { "nothing changed".into() } else { changed.join(", ") });
            Ok(())
        }
        "discover" | "import" => {
            let json_out = args.iter().any(|a| a == "--json");
            let extra: Vec<PathBuf> = args[1..].iter().filter(|a| !a.starts_with("--")).map(|a| expand_tilde(a)).collect();
            let keys = discover(&extra)?;
            if json_out {
                let v: Vec<Value> = keys.iter().map(|k| json!({"file": k.path.display().to_string(), "kind": format!("{:?}", k.kind), "key_id": k.key_id, "fingerprint": k.fingerprint, "imported_from": k.imported_from.as_ref().map(|p| p.display().to_string())})).collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("searched: {}", search_dirs().iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", "));
                if keys.is_empty() {
                    println!("no key file found");
                }
                for k in keys {
                    println!("{:<60} {:?} {} {}", k.path.display(), k.kind, k.key_id.clone().unwrap_or_else(|| "(id?)".into()), k.imported_from.map(|p| format!("(imported from {})", p.display())).unwrap_or_default());
                }
            }
            Ok(())
        }
        "links" => {
            let filter = args.get(1).map(String::as_str);
            for p in PORTALS {
                if filter.map(|f| f == p.id).unwrap_or(true) {
                    println!("{:<17} {}\n{:<17} {}\n", p.id, p.url, "", p.what);
                }
            }
            Ok(())
        }
        "open" => {
            let id = args.get(1).map(String::as_str).unwrap_or("");
            let Some(p) = portal(id) else { anyhow::bail!("unknown portal `{id}`; one of: {}", PORTALS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")) };
            match open_url(p.url) {
                Ok(via) => println!("opened {} (via {via})\n{}", p.url, p.what),
                Err(e) => println!("{e}\nopen this URL yourself: {}\n{}", p.url, p.what),
            }
            Ok(())
        }
        "pubkey" => {
            let section = args.get(1).map(String::as_str).unwrap_or("");
            let (path, public) = ensure_generated_key(section)?;
            println!("private key: {}\npaste this public key in the {} console:\n{public}", path.display(), if section == "cloudkit" { "CloudKit" } else { "Search Ads" });
            Ok(())
        }
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => anyhow::bail!("unknown setup command `{other}`\n{USAGE}"),
    }
}

fn parse_auto_args(args: &[String]) -> anyhow::Result<(AutoOptions, bool)> {
    let mut opts = AutoOptions { want: None, assignments: vec![], extra_paths: vec![], offline: false };
    let mut json_out = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--json" => json_out = true,
            "--offline" => opts.offline = true,
            "--stdin" => {
                for (k, v) in read_stdin_json()?.iter() {
                    match v {
                        Value::Object(sec) => {
                            for (f, fv) in sec {
                                opts.assignments.push((format!("{k}.{f}"), match fv { Value::String(s) => s.clone(), Value::Null => String::new(), o => o.to_string() }));
                            }
                        }
                        Value::String(s) => opts.assignments.push((k.clone(), s.clone())),
                        Value::Null => opts.assignments.push((k.clone(), String::new())),
                        o => opts.assignments.push((k.clone(), o.to_string())),
                    }
                }
            }
            "--want" => {
                i += 1;
                let list = args.get(i).ok_or_else(|| anyhow::anyhow!("--want needs a list"))?;
                let set: BTreeSet<String> = list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                for s in &set {
                    if !SERVICES.iter().any(|x| x.section == s) {
                        anyhow::bail!("unknown section `{s}` in --want; sections: {}", SERVICES.iter().map(|x| x.section).collect::<Vec<_>>().join(", "));
                    }
                }
                opts.want = Some(set);
            }
            "--key" => {
                i += 1;
                opts.extra_paths.push(expand_tilde(args.get(i).ok_or_else(|| anyhow::anyhow!("--key needs a path"))?));
            }
            _ => match parse_kv(a) {
                Some(kv) => opts.assignments.push(kv),
                None => anyhow::bail!("unexpected argument `{a}`\n{USAGE}"),
            },
        }
        i += 1;
    }
    Ok((opts, json_out))
}

async fn run_auto(args: &[String]) -> anyhow::Result<()> {
    let (opts, json_out) = parse_auto_args(args)?;
    let report = auto(&opts).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_report(&report));
    }
    Ok(())
}

fn prompt(label: &str) -> anyhow::Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

/// Interactive loop for a terminal: run auto, ask for each missing item (opening the portal on
/// request), accept pasted PEM blocks, repeat until done or the user stops.
async fn wizard() -> anyhow::Result<()> {
    println!("apple-mcp setup wizard — Enter skips an item, `o` opens the portal in your browser, `q` quits.\n");
    let want_in = prompt("Sections to configure (comma list, Enter = all): ")?;
    let want: Option<BTreeSet<String>> = if want_in.is_empty() { None } else { Some(want_in.split(',').map(|s| s.trim().to_string()).collect()) };
    let mut opts = AutoOptions { want, assignments: vec![], extra_paths: vec![], offline: false };
    loop {
        let report = auto(&opts).await?;
        opts.assignments.clear();
        print!("{}", render_report(&report));
        let todo: Vec<(String, Need, Vec<String>)> = report
            .services
            .iter()
            .flat_map(|s| s.needs.iter().map(move |n| (s.section.clone(), n.clone(), s.choices.get(&n.field).cloned().unwrap_or_default())))
            .collect();
        if todo.is_empty() {
            println!("\nAll requested services are configured.");
            return Ok(());
        }
        println!();
        for (_section, n, choices) in todo {
            println!("{}\n  {}\n  {}", n.field, n.how, n.url);
            if !choices.is_empty() {
                println!("  choices: {}", choices.join(" | "));
            }
            loop {
                let v = prompt(&format!("{} = ", n.field))?;
                match v.as_str() {
                    "q" => return Ok(()),
                    "o" => {
                        match open_url(&n.url) {
                            Ok(_) => println!("  opened in the browser"),
                            Err(e) => println!("  {e}"),
                        }
                        continue;
                    }
                    "" => break,
                    _ => {}
                }
                if v.contains("BEGIN") {
                    // Multi-line PEM: read until END line.
                    let mut pem = v.clone();
                    while !pem.contains("END ") {
                        let l = prompt("")?;
                        pem.push('\n');
                        pem.push_str(&l);
                    }
                    opts.assignments.push((n.field.clone(), pem));
                } else if n.field.ends_with("private_key_path") {
                    let p = expand_tilde(&v);
                    if !p.exists() {
                        println!("  file not found: {}", p.display());
                        continue;
                    }
                    opts.extra_paths.push(p);
                } else {
                    opts.assignments.push((n.field.clone(), v));
                }
                break;
            }
        }
        if opts.assignments.is_empty() && opts.extra_paths.is_empty() {
            println!("Nothing entered; run `apple-mcp setup` again when you have the values.");
            return Ok(());
        }
        println!("\nProbing…\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ids_from_apple_download_names() {
        assert_eq!(key_id_from_name("AuthKey_ABC123DEF4.p8"), (KeyKind::AuthKey, Some("ABC123DEF4".into())));
        assert_eq!(key_id_from_name("AuthKey_ABC123DEF4 (1).p8"), (KeyKind::AuthKey, Some("ABC123DEF4".into())));
        assert_eq!(key_id_from_name("SubscriptionKey_9X8Y7Z6W5V.p8"), (KeyKind::SubscriptionKey, Some("9X8Y7Z6W5V".into())));
        assert_eq!(key_id_from_name("eckey.pem"), (KeyKind::Pem, None));
        assert_eq!(key_id_from_name("AuthKey_short.p8"), (KeyKind::AuthKey, None));
    }

    #[test]
    fn assignments_merge_and_remove() {
        let mut map = Map::new();
        apply_assignments(&mut map, &[("team_id".into(), "T1".into()), ("music.key_id".into(), "K1".into()), ("search_ads.org_id".into(), "42".into())]).unwrap();
        assert_eq!(map["team_id"], "T1");
        assert_eq!(map["music"]["key_id"], "K1");
        assert_eq!(map["search_ads"]["org_id"], 42);
        apply_assignments(&mut map, &[("music.key_id".into(), "".into())]).unwrap();
        assert!(map["music"].get("key_id").is_none());
        assert!(apply_assignments(&mut map, &[("maps.private_key_path".into(), "/nonexistent/x.p8".into())]).is_err());
    }

    #[test]
    fn storefront_from_locale() {
        // Only checks the parsing logic through the env-independent path.
        assert!(["us", "fr", "de", "gb", "ca"].contains(&locale_storefront().as_str()) || locale_storefront().len() == 2);
    }

    #[test]
    fn portals_cover_every_field() {
        for s in SERVICES {
            for f in s.required.iter().chain(s.optional.iter()) {
                assert!(portal(portal_for(s.section, f)).is_some(), "{}.{f}", s.section);
            }
        }
    }
}
