---
name: app-store-connect
description: App Store Connect API reference for the apple plugin — apps, builds, TestFlight groups/testers, App Store versions and review state, customer reviews, bundle IDs/certificates/profiles/devices, users, sales & finance reports, Xcode Cloud, Notary. Load for release management, TestFlight distribution, review replies, reports or any asc_* / notary_* tool.
user-invocable: false
---

# App Store Connect API (tools `asc_*`, `notary_*`)

Base `https://api.appstoreconnect.apple.com`; auth is an ES256 JWT (20 min) minted from
`app_store_connect.key_id/issuer_id/private_key` — handled by the server, cached 15 min.
Individual API keys (no issuer) are supported (`sub: user`). Permissions follow the key's role
(Admin, App Manager, Developer, Marketing, Finance, Sales, Customer Support).

## JSON:API conventions (for `asc_request`)

- Filter: `filter[app]=123`, `filter[platform]=IOS`, `filter[processingState]=VALID`.
- Fields: `fields[builds]=version,uploadedDate` (smaller responses). Include related:
  `include=preReleaseVersion`, `include=app,build`.
- Sort: `sort=-uploadedDate`; paging: `limit=200` max, follow `links.next` (`max_pages`).
- Writes: `{"data": {"type": "...", "attributes": {...}, "relationships": {...}}}` with
  `POST` (create), `PATCH` (update) and `DELETE`; relationship edits use
  `/v1/<type>/<id>/relationships/<rel>` with `{"data": [{"type","id"}]}`.
- Rate limit header: `X-Rate-Limit: user-hour-lim:…;user-hour-rem:…`.

## Common workflows

**Find an app**: `asc_apps` (filter by name/bundle id) → `asc_app` for versions, builds, groups.

**Release status**: `asc_app_store_versions` → `appStoreState`/`appVersionState`
(PREPARE_FOR_SUBMISSION, WAITING_FOR_REVIEW, IN_REVIEW, PENDING_DEVELOPER_RELEASE,
READY_FOR_SALE/READY_FOR_DISTRIBUTION, REJECTED, DEVELOPER_REJECTED). Attach a build to a version:
`asc_request PATCH /v1/appStoreVersions/{id}/relationships/build` body
`{"data": {"type": "builds", "id": "<buildId>"}}`. Submit for review: create
`/v1/reviewSubmissions` then `/v1/reviewSubmissionItems`, then PATCH `submitted: true`.
Release a pending version: `PATCH /v1/appStoreVersions/{id}` `{"data":{"type":"appStoreVersions","id":"…","attributes":{"releaseType":"MANUAL"}}}` then
`POST /v1/appStoreVersionReleaseRequests`.

**Builds**: `asc_builds` (processing: PROCESSING → VALID; expired builds can't be distributed).
Export compliance: `PATCH /v1/builds/{id}` `usesNonExemptEncryption: false`. What's new for
testers: `POST /v1/betaBuildLocalizations`.

**TestFlight**: `asc_testflight_groups` → `asc_testflight_add_tester` (external groups e-mail the
invite once the build passes beta review). Add a build to a group:
`asc_request POST /v1/betaGroups/{id}/relationships/builds` `{"data":[{"type":"builds","id":"…"}]}`.
Submit a build for beta review: `POST /v1/betaAppReviewSubmissions`. Public link: PATCH the
group `publicLinkEnabled: true`.

**Customer reviews**: `asc_customer_reviews` (sort `-createdDate`, filter rating/territory) →
`asc_review_reply` (one response per review; replying again replaces it).

**Localizations / metadata**: `/v1/appStoreVersions/{id}/appStoreVersionLocalizations`
(description, keywords, whatsNew, promotionalText), `/v1/apps/{id}/appInfos` →
`/v1/appInfos/{id}/appInfoLocalizations` (name, subtitle, privacy policy URL), screenshots via
`appScreenshotSets`/`appScreenshots` (upload needs the reservation + upload-operations flow).

**Pricing & availability**: `/v1/apps/{id}/appPriceSchedule`, `/v1/appAvailabilities`.

**In-app purchases & subscriptions**: `/v1/apps/{id}/inAppPurchasesV2`, `/v1/apps/{id}/subscriptionGroups`,
`/v1/subscriptions/{id}/prices`, promotional offers, introductory offers.

**Signing**: `asc_signing kind=certificates|profiles|devices|bundle_ids`; create a device:
`POST /v1/devices` `{"attributes":{"name","platform":"IOS","udid"}}`; register a bundle id:
`POST /v1/bundleIds`; profiles: `POST /v1/profiles` with bundleId/certificates/devices relationships.

**Reports**: `asc_sales_report` (DAILY SALES SUMMARY by default, TSV; needs `vendor_number` and a
Sales/Finance-capable key; data for a day is available the next day),
`asc_finance_report` (monthly, region code e.g. `ZZ` for all). Analytics reports:
`POST /v1/analyticsReportRequests` (accessType ONGOING) → `/v1/analyticsReportRequests/{id}/reports`
→ `/v1/analyticsReports/{id}/instances` → `/v1/analyticsReportInstances/{id}/segments` (download URLs).

**Xcode Cloud**: `asc_xcode_cloud products|workflows|build_runs|build_run|actions|start`;
artifacts/issues/test results via `/v1/ciBuildActions/{id}/artifacts|issues|testResults`.

**Notary**: `notary_submissions`, `notary_submission` (status Accepted/Invalid/Rejected + log JSON).
Uploading a new submission needs `xcrun notarytool` locally (S3 upload); this plugin reads status/logs.

**Users & access**: `asc_users`; invite: `POST /v1/userInvitations`.

## Gotchas

- 401 `NOT_AUTHORIZED` with a valid key → the key was revoked, the clock is off, or the key
  belongs to another team. 403 → role too low, or new agreements to accept in App Store Connect.
- 409 on writes → state conflict (e.g. version already has a build, build expired).
- Many list endpoints cap `limit` at 200; use `max_pages`.
- Prefer `fields[...]` to keep responses small; `compact: true` strips `links`.
- Docs: `apple_docs_index technology=appstoreconnectapi query="<endpoint words>"`.
