//! Apple Search Ads Campaign Management API v5 (OAuth 2 client credentials).

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, query_pairs, render};
use crate::jwt;
use crate::server::AppleServer;

const SECTION: &str = "search_ads";
const BASE: &str = "https://api.searchads.apple.com/api/v5";

impl AppleServer {
    async fn searchads_access_token(&self) -> Result<String, ToolError> {
        if let Some(t) = self.tokens.get("searchads") {
            return Ok(t);
        }
        let cfg = self.cfg();
        let client_id = cfg.require(SECTION, "client_id")?;
        let team = cfg.require(SECTION, "team_id")?;
        let key_id = cfg.require(SECTION, "key_id")?;
        let pem = cfg.private_key(SECTION)?;
        let now = jwt::now();
        let secret = jwt::es256_jwt(&pem, json!({"kid": key_id}), json!({"sub": client_id, "aud": "https://appleid.apple.com", "iat": now, "exp": now + 86400, "iss": team}))?;
        let form = vec![
            ("grant_type".to_string(), "client_credentials".to_string()),
            ("client_id".to_string(), client_id),
            ("client_secret".to_string(), secret),
            ("scope".to_string(), "searchadsorg".to_string()),
        ];
        let v = http::json(self.http.post("https://appleid.apple.com/auth/oauth2/token").form(&form), "Search Ads token").await?;
        let token = v["access_token"].as_str().ok_or_else(|| ToolError::new(format!("no access_token in response: {v}")))?.to_string();
        let ttl = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60).max(60);
        self.tokens.put("searchads", token.clone(), Duration::from_secs(ttl));
        Ok(token)
    }

    pub(crate) async fn searchads_call(&self, method: &str, path: &str, query: &[(String, String)], body: Option<&Value>, org_id: &Option<i64>) -> Result<Value, ToolError> {
        let token = self.searchads_access_token().await?;
        let cfg = self.cfg();
        let org = org_id.or_else(|| cfg.get_i64(SECTION, "org_id"));
        let url = if path.starts_with("http") { path.to_string() } else { format!("{BASE}/{}", path.trim_start_matches('/')) };
        let mut req = self.http.request(http::method(method)?, &url).bearer_auth(token);
        if let Some(o) = org
            && !path.contains("acls") {
                req = req.header("X-AP-Context", format!("orgId={o}"));
            }
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        http::json(req, &format!("Search Ads {} {}", method.to_uppercase(), path)).await
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchadsRequestParams {
    /// GET, POST, PUT, DELETE
    pub method: String,
    /// Path under /api/v5, e.g. `campaigns`, `campaigns/{id}/adgroups`, `reports/campaigns`, `keywords/targeting/find`
    pub path: String,
    /// Query params
    pub query: Option<serde_json::Map<String, Value>>,
    /// JSON body
    pub body: Option<Value>,
    /// Organization id (default config search_ads.org_id)
    pub org_id: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchadsCampaignsParams {
    /// Page size (default 50)
    pub limit: Option<u32>,
    /// Offset
    pub offset: Option<u32>,
    /// Organization id (default from config)
    pub org_id: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchadsReportParams {
    /// campaigns (default) | adgroups | keywords | searchterms | creativesets (adgroups/keywords/searchterms need campaign_id)
    pub level: Option<String>,
    /// Campaign id for ad group / keyword / search term reports
    pub campaign_id: Option<i64>,
    /// Start date YYYY-MM-DD
    pub start_date: String,
    /// End date YYYY-MM-DD
    pub end_date: String,
    /// DAILY (default), HOURLY, WEEKLY, MONTHLY; omit granularity-based series with "none"
    pub granularity: Option<String>,
    /// Time zone: UTC (default) or ORTZ (org time zone)
    pub time_zone: Option<String>,
    /// Max rows (default 50)
    pub limit: Option<u32>,
    /// Order field (default localSpend), e.g. impressions, taps, installs, totalInstalls
    pub order_by: Option<String>,
    /// Organization id (default from config)
    pub org_id: Option<i64>,
}

#[tool_router(router = searchads_router, vis = "pub")]
impl AppleServer {
    /// Generic Apple Search Ads API v5 request (campaigns, ad groups, keywords, creatives, budgets, reports).
    #[tool(annotations(open_world_hint = true))]
    pub async fn searchads_request(&self, Parameters(p): Parameters<SearchadsRequestParams>) -> ToolResult {
        Ok(render(&self.searchads_call(&p.method, &p.path, &query_pairs(&p.query), p.body.as_ref(), &p.org_id).await?))
    }

    /// List the organizations (orgId, name, currency, roles) the API client can access.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn searchads_orgs(&self) -> ToolResult {
        Ok(render(&self.searchads_call("GET", "acls", &[], None, &None).await?))
    }

    /// List campaigns (status, budget, countries, app) of the organization.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn searchads_campaigns(&self, Parameters(p): Parameters<SearchadsCampaignsParams>) -> ToolResult {
        let mut q = vec![("limit".to_string(), p.limit.unwrap_or(50).clamp(1, 1000).to_string())];
        crate::util::opt_num(&mut q, "offset", &p.offset);
        Ok(render(&self.searchads_call("GET", "campaigns", &q, None, &p.org_id).await?))
    }

    /// Performance report (impressions, taps, installs, spend, CPA...) at campaign, ad group, keyword or search-term level for a date range.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn searchads_report(&self, Parameters(p): Parameters<SearchadsReportParams>) -> ToolResult {
        let level = p.level.clone().unwrap_or_else(|| "campaigns".into()).to_lowercase();
        let path = match level.as_str() {
            "campaigns" => "reports/campaigns".to_string(),
            "adgroups" | "keywords" | "searchterms" | "creativesets" => {
                let cid = p.campaign_id.ok_or_else(|| ToolError::new("campaign_id is required for this level"))?;
                format!("reports/campaigns/{cid}/{level}")
            }
            other => crate::bail!("unknown level `{other}`"),
        };
        let mut body = json!({
            "startTime": p.start_date, "endTime": p.end_date,
            "timeZone": p.time_zone.clone().unwrap_or_else(|| "UTC".into()),
            "returnRowTotals": true, "returnRecordsWithNoMetrics": false, "returnGrandTotals": true,
            "selector": {"orderBy": [{"field": p.order_by.clone().unwrap_or_else(|| "localSpend".into()), "sortOrder": "DESCENDING"}], "pagination": {"offset": 0, "limit": p.limit.unwrap_or(50).clamp(1, 1000)}}
        });
        match p.granularity.as_deref().map(|g| g.to_uppercase()) {
            Some(g) if g == "NONE" => {}
            Some(g) => body["granularity"] = json!(g),
            None => body["granularity"] = json!("DAILY"),
        }
        Ok(render(&self.searchads_call("POST", &path, &[], Some(&body), &p.org_id).await?))
    }
}
