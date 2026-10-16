//! App Store Connect API (apps, builds, TestFlight, reviews, signing assets, users, reports, Xcode Cloud) and the Notary API.

use std::io::Read;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, bounded, query_pairs, render, strip_links};
use crate::jwt;
use crate::server::AppleServer;
use crate::util::opt;

const ASC_BASE: &str = "https://api.appstoreconnect.apple.com";
const SECTION: &str = "app_store_connect";

impl AppleServer {
    fn asc_token(&self) -> Result<String, ToolError> {
        let cfg = self.cfg();
        let key_id = cfg.require(SECTION, "key_id")?;
        let pem = cfg.private_key(SECTION)?;
        let issuer = cfg.get_str(SECTION, "issuer_id");
        self.tokens.get_or_mint("asc", Duration::from_secs(15 * 60), || {
            let now = jwt::now();
            let mut claims = json!({"iat": now, "exp": now + 19 * 60, "aud": "appstoreconnect-v1"});
            match &issuer {
                Some(iss) => claims["iss"] = json!(iss),
                None => claims["sub"] = json!("user"), // individual API key
            }
            jwt::es256_jwt(&pem, json!({"kid": key_id, "typ": "JWT"}), claims)
        })
    }

    fn asc_url(path: &str) -> String {
        let p = path.trim();
        if p.starts_with("http://") || p.starts_with("https://") {
            return p.to_string();
        }
        let p = p.trim_start_matches('/');
        if p.starts_with("notary/") {
            return format!("https://appstoreconnect.apple.com/{p}");
        }
        if p.starts_with("v1/") || p.starts_with("v2/") || p.starts_with("v3/") {
            format!("{ASC_BASE}/{p}")
        } else {
            format!("{ASC_BASE}/v1/{p}")
        }
    }

    /// Call the App Store Connect API. Follows `links.next` up to `max_pages`, merging `data`/`included`.
    pub(crate) async fn asc_call(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        max_pages: u32,
        compact: bool,
    ) -> Result<Value, ToolError> {
        let token = self.asc_token()?;
        let mut url = Self::asc_url(path);
        let mut merged: Option<Value> = None;
        let mut pages = 0u32;
        loop {
            let mut req = self.http.request(http::method(method)?, &url).bearer_auth(&token);
            if pages == 0 && !query.is_empty() {
                req = req.query(query);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let what = format!("App Store Connect {} {}", method.to_uppercase(), path);
            let mut v = http::json(req, &what).await?;
            let next = v["links"]["next"].as_str().map(|s| s.to_string());
            if compact {
                strip_links(&mut v);
            }
            pages += 1;
            match merged.as_mut() {
                None => merged = Some(v),
                Some(acc) => {
                    for key in ["data", "included"] {
                        if let (Some(Value::Array(dst)), Some(Value::Array(src))) = (acc.get_mut(key), v.get(key)) {
                            dst.extend(src.iter().cloned());
                        }
                    }
                }
            }
            match next {
                Some(n) if pages < max_pages.max(1) => url = n,
                _ => break,
            }
        }
        let mut out = merged.unwrap_or(Value::Null);
        if pages > 1 {
            out["pages_fetched"] = json!(pages);
        }
        Ok(out)
    }

    async fn asc_get(&self, path: &str, query: &[(String, String)]) -> Result<Value, ToolError> {
        self.asc_call("GET", path, query, None, 1, true).await
    }
}

fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscRequestParams {
    /// HTTP method: GET, POST, PATCH, DELETE
    pub method: String,
    /// Endpoint path, e.g. `/v1/apps`, `v1/builds/123`, `/v1/appStoreVersions/456/relationships/build`, or a full URL (also notary: `/notary/v2/submissions`)
    pub path: String,
    /// Query parameters, e.g. {"filter[app]": "123", "fields[builds]": "version,uploadedDate", "limit": 50, "sort": "-uploadedDate"}
    pub query: Option<serde_json::Map<String, Value>>,
    /// JSON body for POST/PATCH ({"data": {...}} JSON:API envelope)
    pub body: Option<Value>,
    /// Follow pagination links up to N pages (default 1, max 20)
    pub max_pages: Option<u32>,
    /// Strip `links` noise from the response (default true)
    pub compact: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscAppsParams {
    /// Filter by app name (contains)
    pub name: Option<String>,
    /// Filter by bundle id (exact)
    pub bundle_id: Option<String>,
    /// Max apps (default 50)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscAppParams {
    /// App Store Connect app id (numeric, from asc_apps)
    pub app_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscBuildsParams {
    /// App id
    pub app_id: String,
    /// Max builds (default 20)
    pub limit: Option<u32>,
    /// Filter by build version (CFBundleVersion)
    pub version: Option<String>,
    /// Filter by pre-release version (CFBundleShortVersionString), e.g. 2.1.0
    pub pre_release_version: Option<String>,
    /// Filter: PROCESSING, FAILED, INVALID, VALID
    pub processing_state: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscVersionsParams {
    /// App id
    pub app_id: String,
    /// IOS, MAC_OS, TV_OS, VISION_OS
    pub platform: Option<String>,
    /// Filter by app store state, e.g. READY_FOR_SALE, PREPARE_FOR_SUBMISSION, WAITING_FOR_REVIEW, IN_REVIEW, REJECTED
    pub state: Option<String>,
    /// Max versions (default 10)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscReviewsParams {
    /// App id
    pub app_id: String,
    /// Max reviews (default 20)
    pub limit: Option<u32>,
    /// Sort: -createdDate (default), createdDate, -rating, rating
    pub sort: Option<String>,
    /// Territory code filter, e.g. FRA, USA
    pub territory: Option<String>,
    /// Rating filter 1-5
    pub rating: Option<u8>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscReviewReplyParams {
    /// Customer review id
    pub review_id: String,
    /// Public response text (published on the App Store)
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscGroupsParams {
    /// App id (optional: all groups of the team)
    pub app_id: Option<String>,
    /// Max groups (default 50)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscTestersParams {
    /// Beta group id
    pub group_id: Option<String>,
    /// App id
    pub app_id: Option<String>,
    /// Filter by email (exact)
    pub email: Option<String>,
    /// Max testers (default 100)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscAddTesterParams {
    /// Tester email
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    /// Beta group id to add the tester to (external group sends the TestFlight invite)
    pub group_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscRemoveTesterParams {
    /// Beta tester id
    pub tester_id: String,
    /// If given, only remove from this group; otherwise delete the tester from the team entirely
    pub group_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscSigningParams {
    /// bundle_ids | certificates | profiles | devices
    pub kind: String,
    /// Max items (default 100)
    pub limit: Option<u32>,
    /// Text filter: name for profiles/devices/bundle_ids (filter[name]), identifier for bundle_ids, displayName for certificates
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscUsersParams {
    /// Max users (default 100)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscSalesParams {
    /// SALES (default), SUBSCRIPTION, SUBSCRIPTION_EVENT, SUBSCRIBER, NEWSSTAND, PRE_ORDER, INSTALLS, FIRST_ANNUAL
    pub report_type: Option<String>,
    /// SUMMARY (default), DETAILED, SUMMARY_INSTALL_TYPE, SUMMARY_TERRITORY, SUMMARY_CHANNEL
    pub report_sub_type: Option<String>,
    /// DAILY (default), WEEKLY, MONTHLY, YEARLY
    pub frequency: Option<String>,
    /// Report date: YYYY-MM-DD for DAILY/WEEKLY, YYYY-MM for MONTHLY, YYYY for YEARLY (default: yesterday)
    pub report_date: Option<String>,
    /// Vendor number (default from config app_store_connect.vendor_number)
    pub vendor_number: Option<String>,
    /// Report version, e.g. 1_0 (SALES), 1_3 (SUBSCRIPTION), 1_4 (SUBSCRIPTION_EVENT)
    pub version: Option<String>,
    /// Max TSV rows returned (default 300)
    pub max_rows: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscFinanceParams {
    /// Region code, e.g. US, EU, JP, WW, ZZ (ZZ = all regions summary)
    pub region_code: String,
    /// Fiscal month YYYY-MM
    pub report_date: String,
    /// Vendor number (default from config)
    pub vendor_number: Option<String>,
    /// Max TSV rows (default 300)
    pub max_rows: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AscXcodeCloudParams {
    /// products | workflows (needs product_id) | build_runs (needs workflow_id) | build_run (needs build_run_id) | actions (needs build_run_id) | start (needs workflow_id)
    pub action: String,
    pub product_id: Option<String>,
    pub workflow_id: Option<String>,
    pub build_run_id: Option<String>,
    /// Max items for lists (default 20)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotaryListParams {
    /// Max submissions (default 20)
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotaryGetParams {
    /// Submission id (UUID)
    pub submission_id: String,
    /// Also fetch the developer log (issues/warnings) when available (default true)
    pub include_log: Option<bool>,
}

fn gunzip_or_text(bytes: &[u8]) -> String {
    let mut dec = flate2::read::MultiGzDecoder::new(bytes);
    let mut text = String::new();
    if dec.read_to_string(&mut text).is_ok() && !text.is_empty() {
        text
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn head_rows(tsv: &str, max_rows: usize) -> String {
    let lines: Vec<&str> = tsv.lines().collect();
    let shown: Vec<&str> = lines.iter().take(max_rows + 1).copied().collect();
    let mut out = shown.join("\n");
    if lines.len() > max_rows + 1 {
        out.push_str(&format!("\n[... {} more rows not shown; raise max_rows or narrow the report ...]", lines.len() - max_rows - 1));
    }
    bounded(out)
}

#[tool_router(router = asc_router, vis = "pub")]
impl AppleServer {
    /// Generic App Store Connect API request (any endpoint: apps, builds, TestFlight, versions, localizations, pricing, in-app purchases, subscriptions, Game Center, Xcode Cloud, analytics reports, notary...). Use apple_docs_index technology=appstoreconnectapi to find endpoints.
    #[tool(annotations(open_world_hint = true))]
    pub async fn asc_request(&self, Parameters(p): Parameters<AscRequestParams>) -> ToolResult {
        let v = self
            .asc_call(&p.method, &p.path, &query_pairs(&p.query), p.body.as_ref(), p.max_pages.unwrap_or(1).min(20), p.compact.unwrap_or(true))
            .await?;
        Ok(render(&v))
    }

    /// List the team's apps in App Store Connect (id, name, bundle id, SKU, primary locale).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_apps(&self, Parameters(p): Parameters<AscAppsParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(50).clamp(1, 200).to_string();
        let mut query = q(&[("fields[apps]", "name,bundleId,sku,primaryLocale,isOrEverWasMadeForKids,contentRightsDeclaration"), ("limit", &limit)]);
        opt(&mut query, "filter[name]", &p.name);
        opt(&mut query, "filter[bundleId]", &p.bundle_id);
        Ok(render(&self.asc_get("/v1/apps", &query).await?))
    }

    /// App overview: attributes, latest App Store versions (state), latest builds, and TestFlight groups.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_app(&self, Parameters(p): Parameters<AscAppParams>) -> ToolResult {
        let app = self.asc_get(&format!("/v1/apps/{}", p.app_id), &q(&[("fields[apps]", "name,bundleId,sku,primaryLocale")])).await?;
        let versions = self
            .asc_get(
                &format!("/v1/apps/{}/appStoreVersions", p.app_id),
                &q(&[("fields[appStoreVersions]", "platform,versionString,appStoreState,appVersionState,createdDate,releaseType,earliestReleaseDate"), ("limit", "5")]),
            )
            .await?;
        let builds = self
            .asc_get(
                "/v1/builds",
                &q(&[("filter[app]", &p.app_id), ("sort", "-uploadedDate"), ("limit", "5"), ("fields[builds]", "version,uploadedDate,processingState,expired,minOsVersion,usesNonExemptEncryption"), ("include", "preReleaseVersion"), ("fields[preReleaseVersions]", "version,platform")]),
            )
            .await?;
        let groups = self
            .asc_get("/v1/betaGroups", &q(&[("filter[app]", &p.app_id), ("fields[betaGroups]", "name,isInternalGroup,publicLinkEnabled,publicLink,createdDate"), ("limit", "20")]))
            .await?;
        Ok(render(&json!({"app": app["data"], "app_store_versions": versions["data"], "latest_builds": builds["data"], "pre_release_versions": builds["included"], "beta_groups": groups["data"]})))
    }

    /// List builds of an app (newest first) with processing state and pre-release version.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_builds(&self, Parameters(p): Parameters<AscBuildsParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(20).clamp(1, 200).to_string();
        let mut query = q(&[
            ("filter[app]", &p.app_id),
            ("sort", "-uploadedDate"),
            ("limit", &limit),
            ("fields[builds]", "version,uploadedDate,expirationDate,expired,processingState,minOsVersion,usesNonExemptEncryption,buildAudienceType"),
            ("include", "preReleaseVersion"),
            ("fields[preReleaseVersions]", "version,platform"),
        ]);
        opt(&mut query, "filter[version]", &p.version);
        opt(&mut query, "filter[preReleaseVersion.version]", &p.pre_release_version);
        opt(&mut query, "filter[processingState]", &p.processing_state);
        Ok(render(&self.asc_get("/v1/builds", &query).await?))
    }

    /// List App Store versions of an app with their review/release state.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_app_store_versions(&self, Parameters(p): Parameters<AscVersionsParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(10).clamp(1, 200).to_string();
        let mut query = q(&[
            ("fields[appStoreVersions]", "platform,versionString,appStoreState,appVersionState,copyright,releaseType,earliestReleaseDate,createdDate,reviewType"),
            ("limit", &limit),
            ("include", "build"),
            ("fields[builds]", "version,uploadedDate,processingState"),
        ]);
        opt(&mut query, "filter[platform]", &p.platform);
        opt(&mut query, "filter[appStoreState]", &p.state);
        Ok(render(&self.asc_get(&format!("/v1/apps/{}/appStoreVersions", p.app_id), &query).await?))
    }

    /// Customer reviews of an app (rating, title, body, territory, date) with existing developer responses.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_customer_reviews(&self, Parameters(p): Parameters<AscReviewsParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(20).clamp(1, 200).to_string();
        let sort = p.sort.clone().unwrap_or_else(|| "-createdDate".into());
        let mut query = q(&[("limit", &limit), ("sort", &sort), ("include", "response"), ("fields[customerReviews]", "rating,title,body,reviewerNickname,createdDate,territory"), ("fields[customerReviewResponses]", "responseBody,lastModifiedDate,state")]);
        opt(&mut query, "filter[territory]", &p.territory);
        if let Some(r) = p.rating {
            query.push(("filter[rating]".into(), r.to_string()));
        }
        Ok(render(&self.asc_get(&format!("/v1/apps/{}/customerReviews", p.app_id), &query).await?))
    }

    /// Publish (or replace) the developer response to a customer review.
    #[tool(annotations(open_world_hint = true))]
    pub async fn asc_review_reply(&self, Parameters(p): Parameters<AscReviewReplyParams>) -> ToolResult {
        let body = json!({"data": {"type": "customerReviewResponses", "attributes": {"responseBody": p.body}, "relationships": {"review": {"data": {"type": "customerReviews", "id": p.review_id}}}}});
        Ok(render(&self.asc_call("POST", "/v1/customerReviewResponses", &[], Some(&body), 1, true).await?))
    }

    /// TestFlight beta groups (internal/external, public link) for an app or the whole team.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_testflight_groups(&self, Parameters(p): Parameters<AscGroupsParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(50).clamp(1, 200).to_string();
        let mut query = q(&[("fields[betaGroups]", "name,isInternalGroup,publicLinkEnabled,publicLink,publicLinkLimit,publicLinkLimitEnabled,createdDate,hasAccessToAllBuilds,feedbackEnabled"), ("limit", &limit), ("include", "app"), ("fields[apps]", "name,bundleId")]);
        opt(&mut query, "filter[app]", &p.app_id);
        Ok(render(&self.asc_get("/v1/betaGroups", &query).await?))
    }

    /// TestFlight beta testers (email, name, invite state) filtered by group, app or email.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_testflight_testers(&self, Parameters(p): Parameters<AscTestersParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(100).clamp(1, 200).to_string();
        let mut query = q(&[("fields[betaTesters]", "firstName,lastName,email,inviteType,state"), ("limit", &limit)]);
        opt(&mut query, "filter[betaGroups]", &p.group_id);
        opt(&mut query, "filter[apps]", &p.app_id);
        opt(&mut query, "filter[email]", &p.email);
        Ok(render(&self.asc_get("/v1/betaTesters", &query).await?))
    }

    /// Invite a tester to a TestFlight beta group (creates the tester if needed).
    #[tool(annotations(open_world_hint = true))]
    pub async fn asc_testflight_add_tester(&self, Parameters(p): Parameters<AscAddTesterParams>) -> ToolResult {
        let mut attrs = json!({"email": p.email});
        if let Some(f) = &p.first_name { attrs["firstName"] = json!(f); }
        if let Some(l) = &p.last_name { attrs["lastName"] = json!(l); }
        let body = json!({"data": {"type": "betaTesters", "attributes": attrs, "relationships": {"betaGroups": {"data": [{"type": "betaGroups", "id": p.group_id}]}}}});
        Ok(render(&self.asc_call("POST", "/v1/betaTesters", &[], Some(&body), 1, true).await?))
    }

    /// Remove a tester from a beta group, or delete the tester from the team when no group is given.
    #[tool(annotations(destructive_hint = true, open_world_hint = true))]
    pub async fn asc_testflight_remove_tester(&self, Parameters(p): Parameters<AscRemoveTesterParams>) -> ToolResult {
        match &p.group_id {
            Some(g) => {
                let body = json!({"data": [{"type": "betaTesters", "id": p.tester_id}]});
                self.asc_call("DELETE", &format!("/v1/betaGroups/{g}/relationships/betaTesters"), &[], Some(&body), 1, true).await?;
                Ok(format!("Tester {} removed from group {g}.", p.tester_id))
            }
            None => {
                self.asc_call("DELETE", &format!("/v1/betaTesters/{}", p.tester_id), &[], None, 1, true).await?;
                Ok(format!("Tester {} deleted from the team.", p.tester_id))
            }
        }
    }

    /// Code-signing assets: bundle IDs, certificates (with expiration), provisioning profiles, or registered devices.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_signing(&self, Parameters(p): Parameters<AscSigningParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(100).clamp(1, 200).to_string();
        let (path, fields, filter_key) = match p.kind.to_lowercase().replace('-', "_").as_str() {
            "bundle_ids" | "bundleids" | "bundle_id" => ("/v1/bundleIds", "fields[bundleIds]=name,identifier,platform,seedId", "filter[identifier]"),
            "certificates" | "certs" => ("/v1/certificates", "fields[certificates]=name,displayName,certificateType,platform,expirationDate,serialNumber", "filter[displayName]"),
            "profiles" => ("/v1/profiles", "fields[profiles]=name,platform,profileType,profileState,expirationDate,uuid,createdDate", "filter[name]"),
            "devices" => ("/v1/devices", "fields[devices]=name,platform,udid,deviceClass,status,model,addedDate", "filter[name]"),
            other => crate::bail!("unknown kind `{other}` (bundle_ids|certificates|profiles|devices)"),
        };
        let (fk, fv) = fields.split_once('=').unwrap();
        let mut query = q(&[(fk, fv), ("limit", &limit)]);
        if let Some(f) = &p.filter {
            query.push((filter_key.into(), f.clone()));
        }
        Ok(render(&self.asc_get(path, &query).await?))
    }

    /// Team users and their roles in App Store Connect.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_users(&self, Parameters(p): Parameters<AscUsersParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(100).clamp(1, 200).to_string();
        Ok(render(&self.asc_get("/v1/users", &q(&[("fields[users]", "username,firstName,lastName,roles,allAppsVisible,provisioningAllowed"), ("limit", &limit)])).await?))
    }

    /// Download a Sales and Trends report (TSV): units, proceeds, subscriptions... Requires vendor_number and a key with Sales/Finance access.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_sales_report(&self, Parameters(p): Parameters<AscSalesParams>) -> ToolResult {
        let cfg = self.cfg();
        let vendor = match p.vendor_number.clone().or_else(|| cfg.get_str(SECTION, "vendor_number")) {
            Some(v) => v,
            None => crate::bail!("vendor_number is required (App Store Connect > Payments and Financial Reports, top-left). Pass it or set app_store_connect.vendor_number."),
        };
        let frequency = p.frequency.clone().unwrap_or_else(|| "DAILY".into()).to_uppercase();
        let report_type = p.report_type.clone().unwrap_or_else(|| "SALES".into()).to_uppercase();
        let sub_type = p.report_sub_type.clone().unwrap_or_else(|| "SUMMARY".into()).to_uppercase();
        let date = p.report_date.clone().unwrap_or_else(|| (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d").to_string());
        let mut query = q(&[("filter[frequency]", &frequency), ("filter[reportType]", &report_type), ("filter[reportSubType]", &sub_type), ("filter[reportDate]", &date), ("filter[vendorNumber]", &vendor)]);
        opt(&mut query, "filter[version]", &p.version);
        let token = self.asc_token()?;
        let resp = http::send(self.http.get(format!("{ASC_BASE}/v1/salesReports")).bearer_auth(token).query(&query).header("Accept", "application/a-gzip")).await?;
        if !resp.ok() {
            return Err(http::http_error("sales report", &resp));
        }
        let text = gunzip_or_text(&resp.bytes);
        Ok(format!("Sales report {report_type}/{sub_type} {frequency} {date} (vendor {vendor}):\n{}", head_rows(&text, p.max_rows.unwrap_or(300) as usize)))
    }

    /// Download a Finance report (TSV) for a region and fiscal month.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn asc_finance_report(&self, Parameters(p): Parameters<AscFinanceParams>) -> ToolResult {
        let cfg = self.cfg();
        let vendor = match p.vendor_number.clone().or_else(|| cfg.get_str(SECTION, "vendor_number")) {
            Some(v) => v,
            None => crate::bail!("vendor_number is required. Pass it or set app_store_connect.vendor_number."),
        };
        let query = q(&[("filter[regionCode]", &p.region_code), ("filter[reportDate]", &p.report_date), ("filter[reportType]", "FINANCIAL"), ("filter[vendorNumber]", &vendor)]);
        let token = self.asc_token()?;
        let resp = http::send(self.http.get(format!("{ASC_BASE}/v1/financeReports")).bearer_auth(token).query(&query).header("Accept", "application/a-gzip")).await?;
        if !resp.ok() {
            return Err(http::http_error("finance report", &resp));
        }
        let text = gunzip_or_text(&resp.bytes);
        Ok(format!("Finance report {} {} (vendor {vendor}):\n{}", p.region_code, p.report_date, head_rows(&text, p.max_rows.unwrap_or(300) as usize)))
    }

    /// Xcode Cloud: list products, workflows, build runs; inspect a run's actions; or start a workflow build.
    #[tool(annotations(open_world_hint = true))]
    pub async fn asc_xcode_cloud(&self, Parameters(p): Parameters<AscXcodeCloudParams>) -> ToolResult {
        let limit = p.limit.unwrap_or(20).clamp(1, 200).to_string();
        let need = |v: &Option<String>, name: &str| v.clone().ok_or_else(|| ToolError::new(format!("{name} is required for action {}", p.action)));
        let v = match p.action.to_lowercase().as_str() {
            "products" => self.asc_get("/v1/ciProducts", &q(&[("fields[ciProducts]", "name,productType,createdDate"), ("include", "app"), ("fields[apps]", "name,bundleId"), ("limit", &limit)])).await?,
            "workflows" => self.asc_get(&format!("/v1/ciProducts/{}/workflows", need(&p.product_id, "product_id")?), &q(&[("fields[ciWorkflows]", "name,description,isEnabled,isLockedForEditing,lastModifiedDate"), ("limit", &limit)])).await?,
            "build_runs" | "builds" => self.asc_get(&format!("/v1/ciWorkflows/{}/buildRuns", need(&p.workflow_id, "workflow_id")?), &q(&[("sort", "-number"), ("fields[ciBuildRuns]", "number,createdDate,startedDate,finishedDate,sourceCommit,executionProgress,completionStatus,startReason,isPullRequestBuild"), ("limit", &limit)])).await?,
            "build_run" | "run" => self.asc_get(&format!("/v1/ciBuildRuns/{}", need(&p.build_run_id, "build_run_id")?), &q(&[("include", "workflow,builds"), ("fields[builds]", "version,processingState,uploadedDate")])).await?,
            "actions" => self.asc_get(&format!("/v1/ciBuildRuns/{}/actions", need(&p.build_run_id, "build_run_id")?), &q(&[("fields[ciBuildActions]", "name,actionType,startedDate,finishedDate,issueCounts,executionProgress,completionStatus,isRequiredToPass"), ("limit", &limit)])).await?,
            "start" => {
                let body = json!({"data": {"type": "ciBuildRuns", "relationships": {"workflow": {"data": {"type": "ciWorkflows", "id": need(&p.workflow_id, "workflow_id")?}}}}});
                self.asc_call("POST", "/v1/ciBuildRuns", &[], Some(&body), 1, true).await?
            }
            other => crate::bail!("unknown action `{other}` (products|workflows|build_runs|build_run|actions|start)"),
        };
        Ok(render(&v))
    }

    /// Notary service: list recent notarization submissions (status, name, date).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn notary_submissions(&self, Parameters(p): Parameters<NotaryListParams>) -> ToolResult {
        let mut v = self.asc_get("/notary/v2/submissions", &[]).await?;
        if let Some(arr) = v["data"].as_array_mut() {
            arr.truncate(p.limit.unwrap_or(20).clamp(1, 500) as usize);
        }
        Ok(render(&v))
    }

    /// Notary service: status of one submission, with the developer log (issues) when finished.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn notary_submission(&self, Parameters(p): Parameters<NotaryGetParams>) -> ToolResult {
        let status = self.asc_get(&format!("/notary/v2/submissions/{}", p.submission_id), &[]).await?;
        let mut out = json!({"submission": status["data"]});
        if p.include_log.unwrap_or(true) {
            match self.asc_get(&format!("/notary/v2/submissions/{}/logs", p.submission_id), &[]).await {
                Ok(logs) => {
                    if let Some(url) = logs["data"]["attributes"]["developerLogUrl"].as_str() {
                        match http::json(self.http.get(url), "notary developer log").await {
                            Ok(log) => out["developer_log"] = log,
                            Err(e) => out["developer_log_error"] = json!(e.to_string()),
                        }
                    }
                }
                Err(e) => out["log_error"] = json!(e.to_string()),
            }
        }
        Ok(render(&out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_forms() {
        assert_eq!(AppleServer::asc_url("apps"), "https://api.appstoreconnect.apple.com/v1/apps");
        assert_eq!(AppleServer::asc_url("/v1/apps?x=1"), "https://api.appstoreconnect.apple.com/v1/apps?x=1");
        assert_eq!(AppleServer::asc_url("v2/salesReports"), "https://api.appstoreconnect.apple.com/v2/salesReports");
        assert_eq!(AppleServer::asc_url("/notary/v2/submissions"), "https://appstoreconnect.apple.com/notary/v2/submissions");
        assert_eq!(AppleServer::asc_url("https://api.appstoreconnect.apple.com/v1/apps?cursor=x"), "https://api.appstoreconnect.apple.com/v1/apps?cursor=x");
    }

    #[test]
    fn gunzip_fallback() {
        assert_eq!(gunzip_or_text(b"plain text"), "plain text");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, b"a\tb\n1\t2\n").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(gunzip_or_text(&gz), "a\tb\n1\t2\n");
    }
}
