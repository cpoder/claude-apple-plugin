//! Credentials/config loading: JSON file + environment overrides.
//!
//! Lookup order for `section.key`:
//!
//! 1. env `APPLE_MCP_<SECTION>_<KEY>` (upper-case)
//! 2. `config.json` → `section.key`
//!
//! For `team_id`, the top-level `team_id` / `APPLE_TEAM_ID` is the fallback.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::ToolError;

pub const CONFIG_ENV: &str = "APPLE_MCP_CONFIG";

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub path: PathBuf,
    pub exists: bool,
    pub load_error: Option<String>,
    /// Modification time of the file when it was read (None when absent); used for hot reload.
    pub mtime: Option<std::time::SystemTime>,
    data: Map<String, Value>,
}

/// One credential set and the keys it needs.
pub struct ServiceSpec {
    pub section: &'static str,
    pub label: &'static str,
    pub required: &'static [&'static str],
    pub optional: &'static [&'static str],
    pub tools: &'static str,
}

pub const SERVICES: &[ServiceSpec] = &[
    ServiceSpec { section: "app_store_connect", label: "App Store Connect API (+ Notary, Xcode Cloud)", required: &["key_id", "private_key"], optional: &["issuer_id", "vendor_number"], tools: "asc_*, notary_*" },
    ServiceSpec { section: "app_store_server", label: "App Store Server API (in-app purchases)", required: &["key_id", "issuer_id", "bundle_id", "private_key"], optional: &["environment"], tools: "storekit_*" },
    ServiceSpec { section: "music", label: "Apple Music API", required: &["key_id", "team_id", "private_key"], optional: &["user_token", "storefront"], tools: "music_*" },
    ServiceSpec { section: "weatherkit", label: "WeatherKit REST API", required: &["key_id", "service_id", "team_id", "private_key"], optional: &[], tools: "weather_*" },
    ServiceSpec { section: "maps", label: "Apple Maps Server API", required: &["key_id", "team_id", "private_key"], optional: &[], tools: "maps_*" },
    ServiceSpec { section: "cloudkit", label: "CloudKit Web Services (server-to-server)", required: &["container", "key_id", "private_key"], optional: &["environment", "database"], tools: "cloudkit_*" },
    ServiceSpec { section: "apns", label: "Apple Push Notification service", required: &["key_id", "team_id", "private_key", "topic"], optional: &["environment"], tools: "apns_send" },
    ServiceSpec { section: "sign_in_with_apple", label: "Sign in with Apple", required: &["client_id", "team_id", "key_id", "private_key"], optional: &[], tools: "siwa_*" },
    ServiceSpec { section: "business_manager", label: "Apple Business Manager / School Manager API", required: &["client_id", "key_id", "private_key"], optional: &["scope"], tools: "abm_*" },
    ServiceSpec { section: "search_ads", label: "Apple Search Ads API", required: &["client_id", "team_id", "key_id", "private_key", "org_id"], optional: &[], tools: "searchads_*" },
    ServiceSpec { section: "news", label: "Apple News Publisher API", required: &["channel_id", "key_id", "key_secret"], optional: &[], tools: "news_*" },
    ServiceSpec { section: "icloud", label: "iCloud: Calendar/Contacts/Mail (app-specific password) + web services (Apple ID password)", required: &["apple_id", "app_specific_password"], optional: &["password", "mail_from", "mail_from_name"], tools: "icloud_*" },
];

pub const CONFIG_TEMPLATE: &str = r#"{
  "team_id": "ABCDE12345",

  "app_store_connect": {
    "_where": "App Store Connect > Users and Access > Integrations > App Store Connect API > Team Keys (or Individual Keys: then omit issuer_id)",
    "key_id": "XXXXXXXXXX",
    "issuer_id": "69a6de7e-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
    "private_key_path": "~/.config/apple-mcp/AuthKey_XXXXXXXXXX.p8",
    "vendor_number": "optional, for sales/finance reports (App Store Connect > Payments and Financial Reports)"
  },
  "app_store_server": {
    "_where": "App Store Connect > Users and Access > Integrations > In-App Purchase keys",
    "key_id": "", "issuer_id": "", "bundle_id": "com.example.app",
    "private_key_path": "~/.config/apple-mcp/SubscriptionKey_XXXX.p8",
    "environment": "production"
  },
  "music": {
    "_where": "developer.apple.com > Certificates, Identifiers & Profiles > Keys > key with 'Media Services (MusicKit)' enabled",
    "key_id": "", "team_id": "", "private_key_path": "",
    "user_token": "optional Music User Token for /v1/me/* (library, recommendations)",
    "storefront": "fr"
  },
  "weatherkit": {
    "_where": "Keys > key with WeatherKit enabled; Identifiers > Services IDs > service ID with WeatherKit",
    "key_id": "", "service_id": "com.example.weatherkit-client", "team_id": "", "private_key_path": ""
  },
  "maps": {
    "_where": "Keys > key with Maps enabled (Maps IDs are for MapKit JS; the Server API uses the key directly)",
    "key_id": "", "team_id": "", "private_key_path": ""
  },
  "cloudkit": {
    "_where": "CloudKit Console > container > Settings > Tokens & Keys > Server-to-Server Keys (upload the public key of an EC P-256 key: openssl ecparam -name prime256v1 -genkey -noout -out eckey.pem)",
    "container": "iCloud.com.example.app", "key_id": "", "private_key_path": "~/.config/apple-mcp/eckey.pem",
    "environment": "development", "database": "public"
  },
  "apns": {
    "_where": "Keys > key with 'Apple Push Notifications service (APNs)' enabled",
    "key_id": "", "team_id": "", "private_key_path": "", "topic": "com.example.app", "environment": "production"
  },
  "sign_in_with_apple": {
    "_where": "Keys > key with 'Sign in with Apple' enabled; client_id = Services ID (web) or bundle ID (native)",
    "client_id": "com.example.web", "team_id": "", "key_id": "", "private_key_path": ""
  },
  "business_manager": {
    "_where": "Apple Business Manager > Preferences > API > Add (download the .pem key); scope business.api or school.api",
    "client_id": "BUSINESSAPI.xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx", "key_id": "", "private_key_path": "", "scope": "business.api"
  },
  "search_ads": {
    "_where": "Apple Search Ads > Account Settings > API > Create API Client (upload your public key); team_id is the one shown on that page",
    "client_id": "SEARCHADS.xxxxxxxx", "team_id": "SEARCHADS.xxxxxxxx", "key_id": "", "private_key_path": "", "org_id": 123456
  },
  "news": {
    "_where": "News Publisher > channel > API keys (channel ID, key ID, key secret)",
    "channel_id": "", "key_id": "", "key_secret": ""
  },
  "icloud": {
    "_where": "account.apple.com > Sign-In and Security > App-Specific Passwords (needs 2FA). apple_id is the iCloud email.",
    "apple_id": "you@icloud.com", "app_specific_password": "xxxx-xxxx-xxxx-xxxx",
    "password": "optional: the full Apple ID password, only for the iCloud web tools (Reminders, Notes, Drive, Photos, Find My, Hide My Email); one 2FA code at first login",
    "mail_from": "optional sender address (must be an iCloud address or alias), defaults to apple_id",
    "mail_from_name": "optional display name"
  }
}"#;

impl Config {
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var(CONFIG_ENV)
            && !p.trim().is_empty() {
                return expand_tilde(p.trim());
            }
        dirs::config_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
            .join("apple-mcp")
            .join("config.json")
    }

    pub fn load() -> Config {
        let path = Self::default_path();
        let mut cfg = Config { path: path.clone(), exists: path.exists(), load_error: None, mtime: file_mtime(&path), data: Map::new() };
        if cfg.exists {
            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(Value::Object(map)) => cfg.data = map,
                    Ok(_) => cfg.load_error = Some("config root must be a JSON object".into()),
                    Err(e) => cfg.load_error = Some(format!("invalid JSON: {e}")),
                },
                Err(e) => cfg.load_error = Some(format!("cannot read: {e}")),
            }
        }
        cfg
    }

    fn env_name(section: &str, key: &str) -> String {
        format!("APPLE_MCP_{}_{}", section.to_uppercase(), key.to_uppercase())
    }

    /// String value with env override. Numbers are stringified. Empty strings count as missing.
    pub fn get_str(&self, section: &str, key: &str) -> Option<String> {
        if let Ok(v) = std::env::var(Self::env_name(section, key))
            && !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        let v = self.data.get(section)?.as_object()?.get(key)?;
        match v {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    pub fn get_str_or(&self, section: &str, key: &str, default: &str) -> String {
        self.get_str(section, key).unwrap_or_else(|| default.to_string())
    }

    pub fn get_i64(&self, section: &str, key: &str) -> Option<i64> {
        self.get_str(section, key)?.parse().ok()
    }

    pub fn require(&self, section: &str, key: &str) -> Result<String, ToolError> {
        self.get_str(section, key).ok_or_else(|| {
            ToolError::new(format!(
                "Missing `{section}.{key}` in {} (or env {}). Run /apple-services:setup, or call apple_config_template for the expected layout.",
                self.path.display(),
                Self::env_name(section, key)
            ))
        })
    }

    /// Team ID: `section.team_id`, then top-level `team_id`, then `APPLE_TEAM_ID`.
    pub fn team_id(&self, section: &str) -> Result<String, ToolError> {
        if let Some(t) = self.get_str(section, "team_id") {
            return Ok(t);
        }
        if let Ok(t) = std::env::var("APPLE_TEAM_ID")
            && !t.trim().is_empty() {
                return Ok(t.trim().to_string());
            }
        if let Some(Value::String(t)) = self.data.get("team_id")
            && !t.trim().is_empty() {
                return Ok(t.trim().to_string());
            }
        Err(ToolError::new(format!(
            "Missing `{section}.team_id` (or top-level `team_id` / env APPLE_TEAM_ID) in {}",
            self.path.display()
        )))
    }

    fn has_private_key(&self, section: &str) -> bool {
        self.get_str(section, "private_key").is_some()
            || self.get_str(section, "private_key_path").map(|p| expand_tilde(&p).exists()).unwrap_or(false)
    }

    /// PEM private key: inline `private_key` (literal "\n" accepted) or `private_key_path`.
    pub fn private_key(&self, section: &str) -> Result<String, ToolError> {
        if let Some(inline) = self.get_str(section, "private_key") {
            return Ok(inline.replace("\\n", "\n"));
        }
        if let Some(p) = self.get_str(section, "private_key_path") {
            let path = expand_tilde(&p);
            return std::fs::read_to_string(&path).map_err(|e| {
                ToolError::new(format!("Cannot read `{section}.private_key_path` ({}): {e}", path.display()))
            });
        }
        Err(ToolError::new(format!(
            "Missing `{section}.private_key_path` (or inline `private_key`) in {} (env {} / {})",
            self.path.display(),
            Self::env_name(section, "PRIVATE_KEY_PATH"),
            Self::env_name(section, "PRIVATE_KEY")
        )))
    }

    /// Which required keys of a section are missing.
    pub fn missing(&self, spec: &ServiceSpec) -> Vec<&'static str> {
        spec.required
            .iter()
            .filter(|k| {
                if **k == "private_key" {
                    !self.has_private_key(spec.section)
                } else if **k == "team_id" {
                    self.team_id(spec.section).is_err()
                } else {
                    self.get_str(spec.section, k).is_none()
                }
            })
            .copied()
            .collect()
    }

    /// iCloud has two independent credential sets: `apple_id` + `app_specific_password` for
    /// CalDAV/CardDAV/IMAP, and `apple_id` + `password` for the icloud.com web services (whose
    /// session is cached on disk after one two-factor code). Reports both, plus the cached
    /// session's state, without touching the network.
    pub fn icloud_status(&self) -> Value {
        self.icloud_status_in(&crate::tools::icloud_web::session_dir())
    }

    pub(crate) fn icloud_status_in(&self, session_dir: &Path) -> Value {
        let missing_of = |keys: &[&'static str]| -> Vec<&'static str> { keys.iter().filter(|k| self.get_str("icloud", k).is_none()).copied().collect() };
        let dav_missing = missing_of(&["apple_id", "app_specific_password"]);
        let web_missing = missing_of(&["apple_id", "password"]);
        let mut web = serde_json::json!({
            "service": "iCloud web: Reminders, Notes, Drive, Photos, Find My, Hide My Email (apple_id + password; one two-factor code at first login, then a cached session)",
            "configured": web_missing.is_empty(),
            "missing": web_missing,
            "session": "absent",
            "tools": "icloud_login, icloud_2fa, icloud_account, icloud_reminders*, icloud_reminder_*, icloud_notes*, icloud_note, icloud_drive_*, icloud_photos_*, icloud_findmy_*, icloud_hme_*",
        });
        if let Some(id) = self.get_str("icloud", "apple_id") {
            let s = crate::tools::icloud_web::cached_session_in(session_dir, &id);
            web["session"] = Value::String(s.state.into());
            web["session_detail"] = Value::String(s.detail.into());
            web["session_file"] = Value::String(s.file.display().to_string());
        }
        serde_json::json!({
            "dav_imap": {
                "service": "iCloud Calendar (CalDAV), Contacts (CardDAV), Mail (IMAP/SMTP) (apple_id + app_specific_password)",
                "configured": dav_missing.is_empty(),
                "missing": dav_missing,
                "tools": "icloud_calendars, icloud_events, icloud_event_*, icloud_contacts, icloud_contact_*, icloud_mail_*",
            },
            "web": web,
        })
    }

    pub fn status_json(&self) -> Value {
        let services: Vec<Value> = SERVICES
            .iter()
            .map(|s| {
                let missing = self.missing(s);
                let mut entry = serde_json::json!({
                    "section": s.section,
                    "service": s.label,
                    "configured": missing.is_empty(),
                    "missing": missing,
                    "optional": s.optional,
                    "tools": s.tools,
                });
                if s.section == "icloud" {
                    let ic = self.icloud_status();
                    let any = [&ic["dav_imap"], &ic["web"]].iter().any(|v| v["configured"].as_bool().unwrap_or(false));
                    entry["configured"] = Value::Bool(any);
                    entry["note"] = Value::String("two independent credential sets: `dav_imap` (apple_id + app_specific_password) and `web` (apple_id + password; `session` is the state of the cached icloud.com session). `configured` is true when either set is complete; `missing` lists the keys of the DAV/IMAP set only.".into());
                    entry["dav_imap"] = ic["dav_imap"].clone();
                    entry["web"] = ic["web"].clone();
                }
                entry
            })
            .collect();
        serde_json::json!({
            "config_path": self.path.display().to_string(),
            "config_file_exists": self.exists,
            "load_error": self.load_error,
            "public_tools_always_available": "itunes_*, apple_charts, apple_system_status, apple_docs*, apple_technologies, apple_news",
            "services": services,
        })
    }

    pub fn status_report(&self) -> String {
        let mut out = format!(
            "config: {} ({})\n",
            self.path.display(),
            if self.exists { "found" } else { "not found" }
        );
        if let Some(e) = &self.load_error {
            out.push_str(&format!("load error: {e}\n"));
        }
        for s in SERVICES {
            if s.section == "icloud" {
                let ic = self.icloud_status();
                for (name, key, label) in [
                    ("icloud (dav/imap)", "dav_imap", "iCloud Calendar/Contacts/Mail (apple_id + app_specific_password)"),
                    ("icloud (web)", "web", "iCloud Reminders/Notes/Drive/Photos/Find My/Hide My Email (apple_id + password)"),
                ] {
                    let v = &ic[key];
                    let missing: Vec<&str> = v["missing"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    if missing.is_empty() {
                        let session = v["session"].as_str().map(|st| format!("; cached session {st}")).unwrap_or_default();
                        out.push_str(&format!("  [ok]      {name:<22} {label}{session}\n"));
                    } else {
                        out.push_str(&format!("  [missing] {name:<22} {label} (needs: {})\n", missing.join(", ")));
                    }
                }
                continue;
            }
            let missing = self.missing(s);
            if missing.is_empty() {
                out.push_str(&format!("  [ok]      {:<22} {}\n", s.section, s.label));
            } else {
                out.push_str(&format!("  [missing] {:<22} {} (needs: {})\n", s.section, s.label, missing.join(", ")));
            }
        }
        out.push_str("  [ok]      public                 iTunes Search, charts, system status, developer docs, news (no credentials)\n");
        out
    }
}

impl Config {
    /// Known section names (plus the pseudo-section `team_id`).
    pub fn section_spec(section: &str) -> Option<&'static ServiceSpec> {
        SERVICES.iter().find(|s| s.section == section)
    }

    /// Current values of a section with secrets masked (for display).
    pub fn section_masked(&self, section: &str) -> Value {
        let mut out = serde_json::Map::new();
        if let Some(Value::Object(map)) = self.data.get(section) {
            for (k, v) in map {
                let masked = match v {
                    Value::String(s) if is_secret_key(k) => Value::String(mask(s)),
                    other => other.clone(),
                };
                out.insert(k.clone(), masked);
            }
        }
        Value::Object(out)
    }

    /// Merge (or replace) a section in the config file and write it back with mode 0600.
    /// Returns the list of keys that are not documented for this section (written anyway).
    pub fn write_section(path: &Path, section: &str, values: &serde_json::Map<String, Value>, replace: bool, team_id: Option<&str>) -> Result<Vec<String>, ToolError> {
        let mut root: serde_json::Map<String, Value> = if path.exists() {
            let text = std::fs::read_to_string(path).map_err(|e| ToolError::new(format!("cannot read {}: {e}", path.display())))?;
            if text.trim().is_empty() {
                serde_json::Map::new()
            } else {
                match serde_json::from_str::<Value>(&text) {
                    Ok(Value::Object(m)) => m,
                    Ok(_) => return Err(ToolError::new(format!("{} is not a JSON object; fix or delete it first", path.display()))),
                    Err(e) => return Err(ToolError::new(format!("{} contains invalid JSON ({e}); fix or delete it first", path.display()))),
                }
            }
        } else {
            serde_json::Map::new()
        };
        let mut unknown = Vec::new();
        if !section.is_empty() {
            let spec = Self::section_spec(section);
            let mut target: serde_json::Map<String, Value> = match (replace, root.get(section)) {
                (false, Some(Value::Object(existing))) => existing.clone(),
                _ => serde_json::Map::new(),
            };
            for (k, v) in values {
                if k.starts_with('_') {
                    continue; // template comments
                }
                if let Some(spec) = spec {
                    let known = spec.required.contains(&k.as_str()) || spec.optional.contains(&k.as_str()) || k == "team_id" || k == "private_key" || k == "private_key_path";
                    if !known {
                        unknown.push(k.clone());
                    }
                }
                match v {
                    Value::Null => { target.remove(k); }
                    Value::String(s) if s.trim().is_empty() => { target.remove(k); }
                    other => { target.insert(k.clone(), other.clone()); }
                }
            }
            if target.is_empty() {
                root.remove(section);
            } else {
                root.insert(section.to_string(), Value::Object(target));
            }
        }
        if let Some(t) = team_id {
            if t.trim().is_empty() {
                root.remove("team_id");
            } else {
                root.insert("team_id".into(), Value::String(t.trim().to_string()));
            }
        }
        write_private(path, &serde_json::to_string_pretty(&Value::Object(root))?)?;
        Ok(unknown)
    }
}

/// Modification time of a file, None when it does not exist.
pub fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

pub fn is_secret_key(k: &str) -> bool {
    let k = k.to_lowercase();
    k.contains("password") || k.contains("secret") || k.contains("token") || k == "private_key"
}

pub fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 4 {
        "••••".to_string()
    } else {
        format!("{}…{} ({n} chars)", s.chars().take(2).collect::<String>(), s.chars().skip(n - 2).collect::<String>())
    }
}

/// Write a file readable only by the user (0600), creating the parent directory (0700).
pub fn write_private(path: &Path, content: &str) -> Result<(), ToolError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ToolError::new(format!("cannot create {}: {e}", dir.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| ToolError::new(format!("cannot write {}: {e}", tmp.display())))?;
        use std::io::Write;
        f.write_all(content.as_bytes()).map_err(|e| ToolError::new(e.to_string()))?;
        if !content.ends_with('\n') {
            let _ = f.write_all(b"\n");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| ToolError::new(format!("cannot move {} into place: {e}", tmp.display())))?;
    Ok(())
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    Path::new(p).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_merges_and_replaces_sections() {
        let dir = std::env::temp_dir().join(format!("apple-mcp-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let mut v = serde_json::Map::new();
        v.insert("key_id".into(), Value::String("K1".into()));
        v.insert("bogus".into(), Value::String("x".into()));
        v.insert("_where".into(), Value::String("ignored".into()));
        let unknown = Config::write_section(&path, "weatherkit", &v, false, Some("TEAM")).unwrap();
        assert_eq!(unknown, vec!["bogus".to_string()]);
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["weatherkit"]["key_id"], "K1");
        assert_eq!(root["weatherkit"]["bogus"], "x");
        assert!(root["weatherkit"].get("_where").is_none());
        assert_eq!(root["team_id"], "TEAM");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // merge: add service_id, remove bogus via null
        let mut v2 = serde_json::Map::new();
        v2.insert("service_id".into(), Value::String("svc".into()));
        v2.insert("bogus".into(), Value::Null);
        Config::write_section(&path, "weatherkit", &v2, false, None).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["weatherkit"]["key_id"], "K1");
        assert_eq!(root["weatherkit"]["service_id"], "svc");
        assert!(root["weatherkit"].get("bogus").is_none());
        // replace
        let mut v3 = serde_json::Map::new();
        v3.insert("key_id".into(), Value::String("K2".into()));
        Config::write_section(&path, "weatherkit", &v3, true, None).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["weatherkit"]["key_id"], "K2");
        assert!(root["weatherkit"].get("service_id").is_none());
        assert_eq!(root["team_id"], "TEAM");
        assert_eq!(mask("abcdefgh"), "ab…gh (8 chars)");
        assert!(is_secret_key("app_specific_password"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn config_with(data: Value) -> Config {
        Config { path: PathBuf::from("/nonexistent/config.json"), exists: true, load_error: None, mtime: None, data: data.as_object().cloned().unwrap_or_default() }
    }

    fn icloud_entry(cfg: &Config) -> Value {
        cfg.status_json()["services"].as_array().unwrap().iter().find(|s| s["section"] == "icloud").cloned().unwrap()
    }

    #[test]
    fn icloud_status_reports_both_credential_sets() {
        let dir = std::env::temp_dir().join(format!("apple-mcp-status-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Web set only (the common case when the user never generated an app-specific password):
        // the entry must not read as "iCloud not configured".
        let web_only = config_with(serde_json::json!({"icloud": {"apple_id": "status-test@example.invalid", "password": "pw"}}));
        let ic = web_only.icloud_status_in(&dir);
        assert_eq!(ic["dav_imap"]["configured"], false);
        assert_eq!(ic["dav_imap"]["missing"], serde_json::json!(["app_specific_password"]));
        assert_eq!(ic["web"]["configured"], true);
        assert_eq!(ic["web"]["missing"], serde_json::json!([]));
        assert_eq!(ic["web"]["session"], "absent");
        assert_eq!(ic["web"]["session_file"], dir.join("statustestexampleinvalid.json").display().to_string());
        let entry = icloud_entry(&web_only);
        assert_eq!(entry["configured"], true, "either set complete counts as configured");
        assert_eq!(entry["missing"], serde_json::json!(["app_specific_password"]));
        assert!(entry["note"].as_str().unwrap().contains("dav_imap"));
        assert_eq!(entry["web"]["configured"], true);
        assert_eq!(entry["dav_imap"]["configured"], false);
        let report = web_only.status_report();
        assert!(report.contains("[missing] icloud (dav/imap)") && report.contains("(needs: app_specific_password)"), "{report}");
        assert!(report.contains("[ok]      icloud (web)") && report.contains("cached session absent"), "{report}");
        assert!(!report.contains("[missing] icloud  "), "the bare icloud line is replaced by the two sets: {report}");

        let dav_only = config_with(serde_json::json!({"icloud": {"apple_id": "status-test@example.invalid", "app_specific_password": "aaaa-bbbb-cccc-dddd"}}));
        let ic = dav_only.icloud_status_in(&dir);
        assert_eq!(ic["dav_imap"]["configured"], true);
        assert_eq!(ic["web"]["configured"], false);
        assert_eq!(ic["web"]["missing"], serde_json::json!(["password"]));
        let entry = icloud_entry(&dav_only);
        assert_eq!((entry["configured"].clone(), entry["missing"].clone()), (serde_json::json!(true), serde_json::json!([])));

        let none = config_with(serde_json::json!({}));
        let ic = none.icloud_status_in(&dir);
        assert_eq!(ic["dav_imap"]["missing"], serde_json::json!(["apple_id", "app_specific_password"]));
        assert_eq!(ic["web"]["missing"], serde_json::json!(["apple_id", "password"]));
        assert_eq!(ic["web"]["session"], "absent");
        assert!(ic["web"].get("session_file").is_none(), "no apple_id: no file to point at");
        assert_eq!(icloud_entry(&none)["configured"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
