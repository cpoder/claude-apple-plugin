//! Server-level tools: status, config reload, config template.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{CONFIG_TEMPLATE, Config, SERVICES, write_private};
use crate::error::{ToolError, ToolResult};
use crate::http::render;
use crate::server::AppleServer;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfigureParams {
    /// Section to write: app_store_connect | app_store_server | music | weatherkit | maps | cloudkit | apns | sign_in_with_apple | business_manager | search_ads | news | icloud (use apple_config_template for the keys of each)
    pub section: String,
    /// Key/values for the section, e.g. {"key_id": "ABC123", "issuer_id": "…", "private_key_path": "~/Downloads/AuthKey_ABC123.p8"}. Null or empty string removes a key.
    pub values: serde_json::Map<String, Value>,
    /// Private key PEM content (.p8/.pem); it is saved to ~/.config/apple-mcp/keys/<section>.p8 with mode 600 and private_key_path is set automatically
    pub private_key_pem: Option<String>,
    /// Also set the top-level team_id (shared default for all services)
    pub team_id: Option<String>,
    /// Replace the whole section instead of merging into existing values (default false)
    pub replace: Option<bool>,
    /// Immediately verify the credentials with a read-only call after writing (default true)
    pub verify: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TestServiceParams {
    /// Section(s) to test, e.g. ["weatherkit"]; omit to test every configured service
    pub sections: Option<Vec<String>>,
}

#[tool_router(router = meta_router, vis = "pub")]
impl AppleServer {
    /// Show which Apple services have credentials configured, the config file path, and which tool families are usable. Call this first when unsure what is available.
    #[tool(annotations(read_only_hint = true))]
    pub async fn apple_status(&self) -> ToolResult {
        Ok(render(&self.cfg().status_json()))
    }

    /// Re-read the config file from disk and drop cached auth tokens. Rarely needed: the server already reloads the file automatically when it changes on disk.
    #[tool]
    pub async fn apple_reload_config(&self) -> ToolResult {
        let fresh = self.reload_config();
        Ok(format!("Config reloaded.\n{}", fresh.status_report()))
    }

    /// Write credentials for one Apple service into the config file (created with mode 600), optionally saving the private key file, then verify them with a read-only call. Use this instead of editing the JSON by hand.
    #[tool]
    pub async fn apple_configure(&self, Parameters(p): Parameters<ConfigureParams>) -> ToolResult {
        let section = p.section.trim().to_lowercase().replace('-', "_");
        let spec = Config::section_spec(&section).ok_or_else(|| ToolError::new(format!("unknown section `{section}`; valid: {}", SERVICES.iter().map(|s| s.section).collect::<Vec<_>>().join(", "))))?;
        let path = Config::default_path();
        let mut values = p.values.clone();
        let mut saved_key: Option<String> = None;
        if let Some(pem) = p.private_key_pem.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if !pem.contains("-----BEGIN") {
                crate::bail!("private_key_pem must be the PEM text of the key (starts with -----BEGIN PRIVATE KEY----- or -----BEGIN EC PRIVATE KEY-----)");
            }
            crate::jwt::signing_key(&pem.replace("\\n", "\n"))?;
            let key_path = path.parent().map(|d| d.join("keys").join(format!("{section}.p8"))).ok_or_else(|| ToolError::new("bad config path"))?;
            write_private(&key_path, &pem.replace("\\n", "\n"))?;
            values.insert("private_key_path".into(), Value::String(key_path.display().to_string()));
            values.remove("private_key");
            saved_key = Some(key_path.display().to_string());
        }
        if let Some(Value::String(kp)) = values.get("private_key_path") {
            let expanded = crate::config::expand_tilde(kp);
            if !expanded.exists() {
                crate::bail!("private_key_path `{kp}` does not exist (pass private_key_pem with the key content, or a valid path)");
            }
        }
        let unknown = Config::write_section(&path, &section, &values, p.replace.unwrap_or(false), p.team_id.as_deref())?;
        let fresh = self.reload_config();
        let missing = fresh.missing(spec);
        let masked = fresh.section_masked(&section);
        let mut out = json!({
            "written": path.display().to_string(),
            "section": section,
            "service": spec.label,
            "values": masked,
            "complete": missing.is_empty(),
            "still_missing": missing,
            "unknown_keys_written": unknown,
            "private_key_saved_to": saved_key,
        });
        if section == "icloud" {
            // `complete`/`still_missing` describe the DAV/IMAP set; the web set (password + cached session) is independent.
            out["icloud"] = fresh.icloud_status();
        }
        if p.verify.unwrap_or(true) && missing.is_empty() {
            match self.probe_service(&section).await {
                Ok(d) => { out["verified"] = json!(true); out["verification"] = json!(d); }
                Err(e) => { out["verified"] = json!(false); out["verification"] = json!(e.to_string()); }
            }
        }
        Ok(render(&out))
    }

    /// Verify configured Apple credentials with one cheap read-only call per service (all services, or the given sections). Reports exactly which ones work and why the others fail.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn apple_test_service(&self, Parameters(p): Parameters<TestServiceParams>) -> ToolResult {
        let only = p.sections.as_ref().map(|v| v.iter().map(|s| s.trim().to_lowercase().replace('-', "_")).collect::<Vec<_>>());
        let results = self.doctor(only.as_deref()).await;
        let summary = json!({
            "ok": results.iter().filter(|r| r.ok).map(|r| r.section).collect::<Vec<_>>(),
            "failed": results.iter().filter(|r| r.configured && !r.ok).map(|r| r.section).collect::<Vec<_>>(),
            "not_configured": results.iter().filter(|r| !r.configured).map(|r| r.section).collect::<Vec<_>>(),
            "results": results.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        });
        Ok(render(&summary))
    }

    /// Return the JSON template for ~/.config/apple-mcp/config.json: every section, its keys, and where to obtain each credential in Apple's portals.
    #[tool(annotations(read_only_hint = true))]
    pub async fn apple_config_template(&self) -> ToolResult {
        let cfg = self.cfg();
        Ok(format!(
            "Config path: {}\nEnv overrides: APPLE_MCP_<SECTION>_<KEY> (e.g. APPLE_MCP_MUSIC_KEY_ID), APPLE_TEAM_ID, APPLE_MCP_CONFIG (path).\nKeys named private_key_path may be replaced by an inline private_key (PEM). Keep the file chmod 600.\n\n{}",
            cfg.path.display(),
            CONFIG_TEMPLATE
        ))
    }
}
