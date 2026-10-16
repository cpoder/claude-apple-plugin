---
name: sign-in-with-apple
description: Sign in with Apple server-side reference for the apple plugin — generating the client secret JWT, verifying identity tokens against Apple's JWKS, exchanging authorization codes, refreshing and revoking tokens, private relay e-mails. Load when implementing or debugging Sign in with Apple (siwa_* tools).
user-invocable: false
---

# Sign in with Apple (tools `siwa_*`)

Config `sign_in_with_apple`: `client_id` (Services ID for web/Android, or the app bundle id for
native), `team_id`, `key_id` + `.p8` (key enabled for Sign in with Apple, grouped under the
primary App ID).

## Flows

- **Client secret** (`siwa_client_secret`): ES256 JWT `iss`=team, `sub`=client_id,
  `aud`=https://appleid.apple.com, ≤ 180 days. Backends use it for `/auth/token` and `/auth/revoke`.
- **Verify an identity token** (`siwa_verify_identity_token`): fetches `https://appleid.apple.com/auth/keys`,
  checks RS256 signature, `iss`, `aud` (= client_id or `audience`), `exp`, optional `nonce`. Claims:
  `sub` (stable user id per team), `email`, `email_verified`, `is_private_email` (relay address
  `…@privaterelay.appleid.com`), `real_user_status` (0 unsupported, 1 unknown, 2 likely real),
  `auth_time`, `nonce_supported`. The user's name is only delivered once, in the first
  authorization response (not in the token) — store it then.
- **Exchange / refresh** (`siwa_token`): `code` + `redirect_uri` (web) → `access_token`,
  `refresh_token`, `id_token`; later `refresh_token` to validate the session (at most once a day per
  Apple's guidance). `invalid_grant` → code already used, expired (5 min), wrong client_id/redirect.
- **Revoke** (`siwa_revoke`): required when the user deletes their account (App Store Review
  guideline 5.1.1(v)); afterwards the next sign-in behaves like a first sign-in.
- Server-to-server notifications (user disabled e-mail forwarding, deleted the account…) are sent
  as a signed JWT to the endpoint registered on the Services ID; decode with `apple_jws_decode` and
  verify like an identity token.
- Private relay: to e-mail relay addresses, register your sender domains/addresses in Certificates,
  Identifiers & Profiles → Services → Sign in with Apple for Email Communication.

Docs: `apple_docs_index technology=signinwithapplerestapi`, `sign_in_with_apple`.
