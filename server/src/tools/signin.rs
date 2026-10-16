//! Sign in with Apple: client secret generation, identity-token verification, token exchange/revocation.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, render};
use crate::jwt;
use crate::server::AppleServer;

const SECTION: &str = "sign_in_with_apple";
const APPLE_ID: &str = "https://appleid.apple.com";

impl AppleServer {
    pub(crate) fn siwa_client_secret_jwt(&self, days: i64) -> Result<(String, String), ToolError> {
        let cfg = self.cfg();
        let client_id = cfg.require(SECTION, "client_id")?;
        let key_id = cfg.require(SECTION, "key_id")?;
        let team = cfg.team_id(SECTION)?;
        let pem = cfg.private_key(SECTION)?;
        let now = jwt::now();
        let exp = now + days.clamp(1, 180) * 86400;
        let tok = jwt::es256_jwt(&pem, json!({"kid": key_id}), json!({"iss": team, "iat": now, "exp": exp, "aud": APPLE_ID, "sub": client_id}))?;
        Ok((tok, client_id))
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClientSecretParams {
    /// Validity in days, max 180 (default 180)
    pub expires_in_days: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VerifyParams {
    /// The identity token (JWT) returned by Sign in with Apple
    pub identity_token: String,
    /// Expected audience (default config client_id); pass the app bundle id for native sign-in
    pub audience: Option<String>,
    /// Expected nonce (SHA-256 hashed nonce if your client hashed it)
    pub nonce: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TokenParams {
    /// Authorization code from the sign-in flow (exchange)
    pub code: Option<String>,
    /// Refresh token (validate/refresh an existing session)
    pub refresh_token: Option<String>,
    /// Redirect URI used in the web flow (required with code for web clients)
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RevokeParams {
    /// Refresh token or access token to revoke
    pub token: String,
    /// refresh_token (default) or access_token
    pub token_type_hint: Option<String>,
}

#[tool_router(router = siwa_router, vis = "pub")]
impl AppleServer {
    /// Generate the client_secret JWT used by servers for Sign in with Apple token requests (valid up to 180 days).
    #[tool(annotations(read_only_hint = true))]
    pub async fn siwa_client_secret(&self, Parameters(p): Parameters<ClientSecretParams>) -> ToolResult {
        let (tok, client_id) = self.siwa_client_secret_jwt(p.expires_in_days.unwrap_or(180))?;
        Ok(render(&json!({"client_id": client_id, "client_secret": tok, "expires_in_days": p.expires_in_days.unwrap_or(180)})))
    }

    /// Verify a Sign in with Apple identity token against Apple's public keys (signature, issuer, audience, expiry, optional nonce) and return its claims (sub, email, email_verified, is_private_email...).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn siwa_verify_identity_token(&self, Parameters(p): Parameters<VerifyParams>) -> ToolResult {
        use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
        let aud = match &p.audience {
            Some(a) => a.clone(),
            None => self.cfg().require(SECTION, "client_id")?,
        };
        let keys_json = http::json(self.http.get(format!("{APPLE_ID}/auth/keys")), "Apple public keys").await?;
        let jwks: JwkSet = serde_json::from_value(keys_json)?;
        let header = decode_header(&p.identity_token).map_err(|e| ToolError::new(format!("invalid token header: {e}")))?;
        let kid = header.kid.ok_or_else(|| ToolError::new("token header has no kid"))?;
        let jwk = jwks.find(&kid).ok_or_else(|| ToolError::new(format!("no Apple public key with kid {kid} (token may be forged or very old)")))?;
        let key = DecodingKey::from_jwk(jwk).map_err(|e| ToolError::new(format!("bad JWK: {e}")))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[aud.as_str()]);
        validation.set_issuer(&[APPLE_ID]);
        let data = decode::<Value>(&p.identity_token, &key, &validation).map_err(|e| ToolError::new(format!("identity token INVALID: {e}")))?;
        let claims = data.claims;
        if let Some(n) = &p.nonce {
            if claims.get("nonce").and_then(|x| x.as_str()) != Some(n.as_str()) {
                crate::bail!("identity token nonce mismatch (token nonce: {:?})", claims.get("nonce"));
            }
        }
        let mut out = claims.clone();
        if let Some(obj) = out.as_object_mut() {
            for k in ["exp", "iat", "auth_time"] {
                if let Some(ts) = obj.get(k).and_then(|x| x.as_i64()) {
                    obj.insert(format!("{k}_iso"), json!(crate::util::ms_to_iso(ts * 1000)));
                }
            }
        }
        Ok(format!("Identity token VALID (signature, iss, aud={aud}, exp).\n{}", render(&out)))
    }

    /// Exchange an authorization code for tokens, or validate/refresh with a refresh token (Sign in with Apple /auth/token). The id_token is decoded for convenience.
    #[tool(annotations(open_world_hint = true))]
    pub async fn siwa_token(&self, Parameters(p): Parameters<TokenParams>) -> ToolResult {
        let (secret, client_id) = self.siwa_client_secret_jwt(1)?;
        let mut form: Vec<(String, String)> = vec![("client_id".into(), client_id), ("client_secret".into(), secret)];
        match (&p.code, &p.refresh_token) {
            (Some(code), _) => {
                form.push(("grant_type".into(), "authorization_code".into()));
                form.push(("code".into(), code.clone()));
                if let Some(r) = &p.redirect_uri { form.push(("redirect_uri".into(), r.clone())); }
            }
            (None, Some(rt)) => {
                form.push(("grant_type".into(), "refresh_token".into()));
                form.push(("refresh_token".into(), rt.clone()));
            }
            _ => crate::bail!("provide code (exchange) or refresh_token (refresh)"),
        }
        let mut v = http::json(self.http.post(format!("{APPLE_ID}/auth/token")).form(&form), "Sign in with Apple token").await?;
        if let Some(idt) = v.get("id_token").and_then(|t| t.as_str()) {
            if let Ok((_, payload)) = jwt::decode_unverified(idt) {
                v["id_token_decoded"] = payload;
            }
        }
        Ok(render(&v))
    }

    /// Revoke a user's Sign in with Apple tokens (required when a user deletes their account).
    #[tool(annotations(destructive_hint = true, open_world_hint = true))]
    pub async fn siwa_revoke(&self, Parameters(p): Parameters<RevokeParams>) -> ToolResult {
        let (secret, client_id) = self.siwa_client_secret_jwt(1)?;
        let form = vec![
            ("client_id".to_string(), client_id),
            ("client_secret".to_string(), secret),
            ("token".to_string(), p.token.clone()),
            ("token_type_hint".to_string(), p.token_type_hint.clone().unwrap_or_else(|| "refresh_token".into())),
        ];
        let resp = http::send(self.http.post(format!("{APPLE_ID}/auth/revoke")).form(&form)).await?;
        if resp.ok() {
            Ok("Token revoked.".into())
        } else {
            Err(http::http_error("Sign in with Apple revoke", &resp))
        }
    }

    /// Fetch Apple's current public keys (JWKS) used to sign identity tokens.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn siwa_keys(&self) -> ToolResult {
        Ok(render(&http::json(self.http.get(format!("{APPLE_ID}/auth/keys")), "Apple public keys").await?))
    }
}
