---
name: apple-search-ads
description: Apple Search Ads Campaign Management API v5 reference for the apple plugin — organizations, campaigns, ad groups, keywords, budgets, and performance reports (impressions, taps, installs, spend). Load for App Store advertising questions or searchads_* tools.
user-invocable: false
---

# Apple Search Ads API v5 (tools `searchads_*`)

Auth: OAuth 2 client credentials — the server builds the `client_secret` JWT (ES256, `sub` =
client id, `iss` = Search Ads team id, `aud` https://appleid.apple.com) and exchanges it at
`https://appleid.apple.com/auth/oauth2/token` with scope `searchadsorg`; the access token lasts
1 h. Requests carry `X-AP-Context: orgId=<org_id>` (except `/acls`). Config `search_ads.*`
(client_id, team_id, key_id as shown on the API client page, private key, org_id).

## Tools

- `searchads_orgs` → `/acls`: orgs the client can access (`orgId`, `orgName`, `currency`,
  `roleNames`, `paymentModel`).
- `searchads_campaigns` → campaigns (`id`, `name`, `status` ENABLED|PAUSED, `servingStatus`,
  `budgetAmount`, `dailyBudgetAmount`, `countriesOrRegions`, `adamId` (app), `adChannelType`
  SEARCH|DISPLAY, `supplySources` APPSTORE_SEARCH_RESULTS|APPSTORE_SEARCH_TAB|APPSTORE_TODAY_TAB|APPSTORE_PRODUCT_PAGES_BROWSE).
- `searchads_report level=campaigns|adgroups|keywords|searchterms|creativesets start_date end_date granularity=DAILY|HOURLY|WEEKLY|MONTHLY|none order_by=localSpend|impressions|taps|installs|totalInstalls`
  → `row[]` with `metadata` (names/ids) and `total`/`granularity[]` metrics: `impressions`, `taps`,
  `installs` (attributed), `newDownloads`, `redownloads`, `latOnInstalls`, `latOffInstalls`, `ttr`,
  `avgCPA`, `avgCPT`, `avgCPM`, `localSpend`, `conversionRate`, `tapInstalls`, `viewInstalls`,
  `totalInstalls`, `totalAvgCPI`. Reports are in the org currency; `time_zone=ORTZ` for org local time.
- Anything else via `searchads_request`: `GET campaigns/{id}/adgroups`, `GET campaigns/{id}/adgroups/{id}/targetingkeywords`,
  `POST campaigns/{id}/adgroups/{id}/negativekeywords/bulk`, `PUT campaigns/{id}` (budget/status),
  `POST keywords/targeting/find` (keyword search), `POST reports/campaigns/{id}/searchterms`,
  `GET apps?query=…` (apps you can advertise), `GET geolocations`.

Pagination: `selector.pagination {offset, limit}` in bodies, `limit/offset` query params on GETs;
response `pagination.totalResults`. Errors: 401 `INVALID_CLIENT` → key/secret mismatch; 403 → org
role; 400 with `errors[].messageCode`. Docs: `apple_docs_index technology=apple_search_ads`.
