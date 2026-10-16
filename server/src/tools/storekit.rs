//! App Store Server API (transactions, subscription status, refunds, notifications) + JWS decoding.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, query_pairs, render};
use crate::jwt;
use crate::server::AppleServer;
use crate::util::annotate_ms_dates;

const SECTION: &str = "app_store_server";

impl AppleServer {
    fn storekit_token(&self) -> Result<String, ToolError> {
        let cfg = self.cfg();
        let key_id = cfg.require(SECTION, "key_id")?;
        let issuer = cfg.require(SECTION, "issuer_id")?;
        let bundle_id = cfg.require(SECTION, "bundle_id")?;
        let pem = cfg.private_key(SECTION)?;
        self.tokens.get_or_mint("storekit", Duration::from_secs(45 * 60), || {
            let now = jwt::now();
            jwt::es256_jwt(
                &pem,
                json!({"kid": key_id, "typ": "JWT"}),
                json!({"iss": issuer, "iat": now, "exp": now + 55 * 60, "aud": "appstoreconnect-v1", "bid": bundle_id}),
            )
        })
    }

    fn storekit_base(&self, environment: &Option<String>) -> (String, String) {
        let env = environment.clone().unwrap_or_else(|| self.cfg().get_str_or(SECTION, "environment", "production")).to_lowercase();
        if env.starts_with("sandbox") {
            ("https://api.storekit-sandbox.itunes.apple.com".into(), "sandbox".into())
        } else {
            ("https://api.storekit.itunes.apple.com".into(), "production".into())
        }
    }

    pub(crate) async fn storekit_call(&self, method: &str, path: &str, query: &[(String, String)], body: Option<&Value>, environment: &Option<String>) -> Result<Value, ToolError> {
        let token = self.storekit_token()?;
        let (base, env) = self.storekit_base(environment);
        let url = if path.starts_with("http") { path.to_string() } else { format!("{base}/{}", path.trim_start_matches('/')) };
        let mut req = self.http.request(http::method(method)?, &url).bearer_auth(token);
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let mut v = http::json(req, &format!("App Store Server API ({env}) {} {}", method.to_uppercase(), path)).await?;
        decode_signed_fields(&mut v, 0);
        v["_environment"] = json!(env);
        Ok(v)
    }
}

const SIGNED_KEYS: &[&str] = &["signedTransactionInfo", "signedRenewalInfo", "signedPayload", "signedTransactions", "signedRenewalInfos"];

/// Replace signed JWS strings in StoreKit responses with their decoded payloads (unverified) and annotate ms dates.
pub fn decode_signed_fields(v: &mut Value, depth: u8) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let is_signed = SIGNED_KEYS.contains(&k.as_str());
                let entry = map.get_mut(&k).unwrap();
                if is_signed {
                    match entry {
                        Value::String(s) => {
                            if let Ok((_, mut payload)) = jwt::decode_unverified(s) {
                                decode_signed_fields(&mut payload, depth + 1);
                                annotate_ms_dates(&mut payload);
                                *entry = payload;
                            }
                        }
                        Value::Array(arr) => {
                            for item in arr.iter_mut() {
                                if let Value::String(s) = item {
                                    if let Ok((_, mut payload)) = jwt::decode_unverified(s) {
                                        decode_signed_fields(&mut payload, depth + 1);
                                        annotate_ms_dates(&mut payload);
                                        *item = payload;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    decode_signed_fields(entry, depth + 1);
                }
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(|x| decode_signed_fields(x, depth + 1)),
        _ => {}
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StorekitRequestParams {
    /// GET, POST, PUT
    pub method: String,
    /// Path, e.g. `/inApps/v1/subscriptions/{transactionId}`, `/inApps/v2/history/{transactionId}`, `/inApps/v1/notifications/history`
    pub path: String,
    /// Query params
    pub query: Option<serde_json::Map<String, Value>>,
    /// JSON body
    pub body: Option<Value>,
    /// production | sandbox (default from config)
    pub environment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TransactionParams {
    /// Transaction id (original or any transaction id of the customer)
    pub transaction_id: String,
    /// production | sandbox
    pub environment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    /// Any transaction id of the customer
    pub transaction_id: String,
    /// production | sandbox
    pub environment: Option<String>,
    /// Pagination token from a previous response
    pub revision: Option<String>,
    /// ASCENDING | DESCENDING
    pub sort: Option<String>,
    /// Filter: AUTO_RENEWABLE, NON_RENEWABLE, CONSUMABLE, NON_CONSUMABLE
    pub product_type: Option<String>,
    /// Filter by product id
    pub product_id: Option<String>,
    /// Start date (ms since epoch)
    pub start_date: Option<i64>,
    /// End date (ms since epoch)
    pub end_date: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OrderLookupParams {
    /// Customer's order id from the App Store receipt email
    pub order_id: String,
    /// production | sandbox
    pub environment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotificationHistoryParams {
    /// Start of range (ms since epoch, within the last 180 days)
    pub start_date: i64,
    /// End of range (ms since epoch)
    pub end_date: i64,
    /// Filter by transaction id (then notification_type must be omitted)
    pub transaction_id: Option<String>,
    /// Filter by type, e.g. DID_RENEW, EXPIRED, REFUND, SUBSCRIBED
    pub notification_type: Option<String>,
    /// Only include notifications Apple failed to deliver (default false)
    pub only_failures: Option<bool>,
    /// Pagination token
    pub pagination_token: Option<String>,
    /// production | sandbox
    pub environment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TestNotificationParams {
    /// production | sandbox
    pub environment: Option<String>,
    /// If given, check the delivery status of a previous test token instead of sending a new one
    pub test_notification_token: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JwsParams {
    /// Compact JWS/JWT string (e.g. signedPayload of an App Store Server Notification, signedTransactionInfo, a StoreKit 2 transaction JWS)
    pub jws: String,
    /// Include the x5c certificate chain in the header output (default false)
    pub include_certificates: Option<bool>,
}

#[tool_router(router = storekit_router, vis = "pub")]
impl AppleServer {
    /// Generic App Store Server API request (in-app purchase transactions, subscriptions, refunds, notifications, consumption info...). Signed JWS fields are decoded automatically.
    #[tool(annotations(open_world_hint = true))]
    pub async fn storekit_request(&self, Parameters(p): Parameters<StorekitRequestParams>) -> ToolResult {
        Ok(render(&self.storekit_call(&p.method, &p.path, &query_pairs(&p.query), p.body.as_ref(), &p.environment).await?))
    }

    /// Get one in-app purchase transaction (decoded): product, purchase/expiration dates, type, offer, revocation.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn storekit_transaction(&self, Parameters(p): Parameters<TransactionParams>) -> ToolResult {
        Ok(render(&self.storekit_call("GET", &format!("/inApps/v1/transactions/{}", p.transaction_id), &[], None, &p.environment).await?))
    }

    /// Customer's in-app purchase transaction history (decoded, paginated with revision).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn storekit_history(&self, Parameters(p): Parameters<HistoryParams>) -> ToolResult {
        let mut q: Vec<(String, String)> = Vec::new();
        crate::util::opt(&mut q, "revision", &p.revision);
        crate::util::opt(&mut q, "sort", &p.sort);
        crate::util::opt(&mut q, "productType", &p.product_type);
        crate::util::opt(&mut q, "productId", &p.product_id);
        crate::util::opt_num(&mut q, "startDate", &p.start_date);
        crate::util::opt_num(&mut q, "endDate", &p.end_date);
        Ok(render(&self.storekit_call("GET", &format!("/inApps/v2/history/{}", p.transaction_id), &q, None, &p.environment).await?))
    }

    /// Status of all auto-renewable subscriptions of a customer (decoded last transaction + renewal info per subscription group).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn storekit_subscriptions(&self, Parameters(p): Parameters<TransactionParams>) -> ToolResult {
        Ok(render(&self.storekit_call("GET", &format!("/inApps/v1/subscriptions/{}", p.transaction_id), &[], None, &p.environment).await?))
    }

    /// Look up the transactions behind a customer's order id (from their receipt email) — useful for support requests.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn storekit_order_lookup(&self, Parameters(p): Parameters<OrderLookupParams>) -> ToolResult {
        Ok(render(&self.storekit_call("GET", &format!("/inApps/v1/lookup/{}", p.order_id), &[], None, &p.environment).await?))
    }

    /// Refund history of a customer (decoded refunded transactions).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn storekit_refund_history(&self, Parameters(p): Parameters<TransactionParams>) -> ToolResult {
        Ok(render(&self.storekit_call("GET", &format!("/inApps/v2/refund/lookup/{}", p.transaction_id), &[], None, &p.environment).await?))
    }

    /// App Store Server Notifications v2 history (decoded) for a date range, optionally filtered by transaction or type.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn storekit_notification_history(&self, Parameters(p): Parameters<NotificationHistoryParams>) -> ToolResult {
        let mut body = json!({"startDate": p.start_date, "endDate": p.end_date});
        if let Some(t) = &p.transaction_id { body["transactionId"] = json!(t); }
        if let Some(t) = &p.notification_type { body["notificationType"] = json!(t); }
        if let Some(f) = p.only_failures { body["onlyFailures"] = json!(f); }
        let mut q: Vec<(String, String)> = Vec::new();
        crate::util::opt(&mut q, "paginationToken", &p.pagination_token);
        Ok(render(&self.storekit_call("POST", "/inApps/v1/notifications/history", &q, Some(&body), &p.environment).await?))
    }

    /// Ask Apple to send a TEST server notification to your configured notification URL, or check the status of a previous test token.
    #[tool(annotations(open_world_hint = true))]
    pub async fn storekit_test_notification(&self, Parameters(p): Parameters<TestNotificationParams>) -> ToolResult {
        match &p.test_notification_token {
            Some(tok) => Ok(render(&self.storekit_call("GET", &format!("/inApps/v1/notifications/test/{tok}"), &[], None, &p.environment).await?)),
            None => Ok(render(&self.storekit_call("POST", "/inApps/v1/notifications/test", &[], None, &p.environment).await?)),
        }
    }

    /// Decode any Apple JWS/JWT (App Store Server Notification signedPayload, StoreKit 2 transaction, Sign in with Apple id_token...) WITHOUT verifying the signature. Millisecond dates get *_iso companions.
    #[tool(annotations(read_only_hint = true))]
    pub async fn apple_jws_decode(&self, Parameters(p): Parameters<JwsParams>) -> ToolResult {
        let (mut header, mut payload) = jwt::decode_unverified(&p.jws)?;
        if !p.include_certificates.unwrap_or(false) {
            if let Some(obj) = header.as_object_mut() {
                if let Some(x5c) = obj.get("x5c").and_then(|x| x.as_array()) {
                    let n = x5c.len();
                    obj.insert("x5c".into(), json!(format!("[{n} certificates omitted; pass include_certificates=true]")));
                }
            }
        }
        decode_signed_fields(&mut payload, 0);
        annotate_ms_dates(&mut payload);
        Ok(render(&json!({"_note": "signature NOT verified", "header": header, "payload": payload})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::prelude::*;

    fn fake_jws(payload: Value) -> String {
        let h = BASE64_URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256"}"#);
        let p = BASE64_URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        format!("{h}.{p}.sig")
    }

    #[test]
    fn decodes_nested_signed_fields() {
        let inner = fake_jws(json!({"productId": "pro", "purchaseDate": 1700000000000i64}));
        let outer = fake_jws(json!({"notificationType": "DID_RENEW", "data": {"signedTransactionInfo": inner}}));
        let mut v = json!({"notificationHistory": [{"signedPayload": outer}]});
        decode_signed_fields(&mut v, 0);
        let tx = &v["notificationHistory"][0]["signedPayload"]["data"]["signedTransactionInfo"];
        assert_eq!(tx["productId"], "pro");
        assert!(tx["purchaseDate_iso"].as_str().unwrap().starts_with("2023-11-14"));
    }
}
