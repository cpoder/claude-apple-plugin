//! CloudKit Web Services with server-to-server authentication (public database).

use base64::prelude::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, render};
use crate::jwt;
use crate::server::AppleServer;

const SECTION: &str = "cloudkit";
const HOST: &str = "https://api.apple-cloudkit.com";

impl AppleServer {
    /// Sign and send a CloudKit Web Services request (all operations are POST with a JSON body).
    pub(crate) async fn cloudkit_call(&self, subpath: &str, body: &Value, database: &Option<String>, environment: &Option<String>) -> Result<Value, ToolError> {
        let cfg = self.cfg();
        let container = cfg.require(SECTION, "container")?;
        let key_id = cfg.require(SECTION, "key_id")?;
        let pem = cfg.private_key(SECTION)?;
        let env = environment.clone().unwrap_or_else(|| cfg.get_str_or(SECTION, "environment", "development"));
        let db = database.clone().unwrap_or_else(|| cfg.get_str_or(SECTION, "database", "public"));
        let path = format!("/database/1/{container}/{env}/{db}/{}", subpath.trim_start_matches('/'));
        let body_str = serde_json::to_string(body)?;
        let date = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let body_hash = BASE64_STANDARD.encode(Sha256::digest(body_str.as_bytes()));
        let message = format!("{date}:{body_hash}:{path}");
        let signature = BASE64_STANDARD.encode(jwt::ecdsa_sign_der(&pem, message.as_bytes())?);
        let req = self
            .http
            .post(format!("{HOST}{path}"))
            .header("Content-Type", "application/json")
            .header("X-Apple-CloudKit-Request-KeyID", key_id)
            .header("X-Apple-CloudKit-Request-ISO8601Date", date)
            .header("X-Apple-CloudKit-Request-SignatureV1", signature)
            .body(body_str);
        let v = http::json(req, &format!("CloudKit {subpath}")).await?;
        if let Some(code) = v.get("serverErrorCode").and_then(|c| c.as_str()) {
            crate::bail!("CloudKit {subpath}: {code} — {}", v.get("reason").and_then(|r| r.as_str()).unwrap_or(""));
        }
        Ok(v)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloudkitRequestParams {
    /// Operation subpath, e.g. `records/query`, `records/lookup`, `records/modify`, `zones/list`, `records/changes`, `assets/upload`
    pub subpath: String,
    /// JSON request body as documented in CloudKit Web Services Reference
    pub body: Value,
    /// public (default; server-to-server keys only access the public database)
    pub database: Option<String>,
    /// development | production (default from config)
    pub environment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloudkitQueryParams {
    /// Record type name
    pub record_type: String,
    /// Filters: [{"comparator": "EQUALS", "fieldName": "name", "fieldValue": {"value": "x"}}] (comparators: EQUALS, NOT_EQUALS, LESS_THAN, GREATER_THAN, LESS_THAN_OR_EQUALS, GREATER_THAN_OR_EQUALS, NEAR, CONTAINS_ALL_TOKENS, IN, NOT_IN, CONTAINS_ANY_TOKENS, LIST_CONTAINS, NOT_LIST_CONTAINS, BEGINS_WITH, NOT_BEGINS_WITH, LIST_MEMBER_BEGINS_WITH, LIST_CONTAINS_ALL, LIST_CONTAINS_ANY)
    pub filter_by: Option<Vec<Value>>,
    /// Sorts: [{"fieldName": "createdAt", "ascending": false}]
    pub sort_by: Option<Vec<Value>>,
    /// Field names to return (default all)
    pub desired_keys: Option<Vec<String>>,
    /// Max records (default 50, max 200)
    pub results_limit: Option<u32>,
    /// Zone name (default _defaultZone)
    pub zone_name: Option<String>,
    /// Continuation marker from a previous response
    pub continuation_marker: Option<String>,
    /// development | production
    pub environment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloudkitLookupParams {
    /// Record names to fetch
    pub record_names: Vec<String>,
    /// Zone name (default _defaultZone)
    pub zone_name: Option<String>,
    /// Field names to return
    pub desired_keys: Option<Vec<String>>,
    /// development | production
    pub environment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloudkitModifyParams {
    /// Operations: [{"operationType": "create|update|forceUpdate|replace|forceReplace|delete|forceDelete", "record": {"recordType": "T", "recordName": "optional", "recordChangeTag": "for update", "fields": {"title": {"value": "x"}}}}]
    pub operations: Vec<Value>,
    /// Zone name (default _defaultZone)
    pub zone_name: Option<String>,
    /// Apply all-or-nothing (default true)
    pub atomic: Option<bool>,
    /// development | production
    pub environment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloudkitZonesParams {
    /// development | production
    pub environment: Option<String>,
}

#[tool_router(router = cloudkit_router, vis = "pub")]
impl AppleServer {
    /// Generic CloudKit Web Services operation (signed server-to-server request to the container's public database).
    #[tool(annotations(open_world_hint = true))]
    pub async fn cloudkit_request(&self, Parameters(p): Parameters<CloudkitRequestParams>) -> ToolResult {
        Ok(render(&self.cloudkit_call(&p.subpath, &p.body, &p.database, &p.environment).await?))
    }

    /// Query records of a type in the CloudKit public database with filters and sorts.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn cloudkit_query(&self, Parameters(p): Parameters<CloudkitQueryParams>) -> ToolResult {
        let mut query = json!({"recordType": p.record_type});
        if let Some(f) = &p.filter_by { query["filterBy"] = json!(f); }
        if let Some(s) = &p.sort_by { query["sortBy"] = json!(s); }
        let mut body = json!({"zoneID": {"zoneName": p.zone_name.clone().unwrap_or_else(|| "_defaultZone".into())}, "query": query, "resultsLimit": p.results_limit.unwrap_or(50).clamp(1, 200)});
        if let Some(k) = &p.desired_keys { body["desiredKeys"] = json!(k); }
        if let Some(c) = &p.continuation_marker { body["continuationMarker"] = json!(c); }
        Ok(render(&self.cloudkit_call("records/query", &body, &None, &p.environment).await?))
    }

    /// Fetch records by name from the CloudKit public database.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn cloudkit_lookup(&self, Parameters(p): Parameters<CloudkitLookupParams>) -> ToolResult {
        let records: Vec<Value> = p.record_names.iter().map(|n| json!({"recordName": n})).collect();
        let mut body = json!({"records": records, "zoneID": {"zoneName": p.zone_name.clone().unwrap_or_else(|| "_defaultZone".into())}});
        if let Some(k) = &p.desired_keys { body["desiredKeys"] = json!(k); }
        Ok(render(&self.cloudkit_call("records/lookup", &body, &None, &p.environment).await?))
    }

    /// Create, update or delete records in the CloudKit public database.
    #[tool(annotations(destructive_hint = true, open_world_hint = true))]
    pub async fn cloudkit_modify(&self, Parameters(p): Parameters<CloudkitModifyParams>) -> ToolResult {
        let body = json!({"operations": p.operations, "zoneID": {"zoneName": p.zone_name.clone().unwrap_or_else(|| "_defaultZone".into())}, "atomic": p.atomic.unwrap_or(true)});
        Ok(render(&self.cloudkit_call("records/modify", &body, &None, &p.environment).await?))
    }

    /// List record zones of the public database.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn cloudkit_zones(&self, Parameters(p): Parameters<CloudkitZonesParams>) -> ToolResult {
        Ok(render(&self.cloudkit_call("zones/list", &json!({}), &None, &p.environment).await?))
    }
}
