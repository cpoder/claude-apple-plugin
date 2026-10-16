---
name: apple-services
description: Map of the Apple plugin — which MCP tool family to use for App Store Connect, TestFlight, App Store Server (in-app purchases), Apple Music, WeatherKit, Apple Maps, CloudKit, push notifications (APNs), Sign in with Apple, Apple Business/School Manager, Search Ads, Apple News, iCloud calendar/contacts/mail, public iTunes/App Store data and Apple developer documentation. Load when a task touches any Apple service or API.
user-invocable: false
---

# Apple services (plugin `apple`)

The `apple` MCP server (Rust) exposes ~100 tools. Public tools need no credentials; the
others read `~/.config/apple-mcp/config.json` (one section per service). When unsure what
is configured, call `apple_status` first. To add or fix credentials use `apple_configure`
(writes one section safely, saves a pasted `.p8`, verifies the service) and
`apple_test_service` (read-only probe of every configured service with Apple's exact error);
`apple_config_template` documents every key and where to get it; `/apple:setup` guides the user.

## Pick the tool family

| Need | Tools | Credentials section |
| :- | :- | :- |
| App Store search, app metadata, charts, Apple outages, developer docs, Apple news | `itunes_search`, `itunes_lookup`, `apple_charts`, `apple_system_status`, `apple_docs`, `apple_docs_index`, `apple_technologies`, `apple_news` | none |
| Apps, builds, TestFlight, App Store versions, reviews, signing assets, users, sales/finance reports, Xcode Cloud, notarization | `asc_*`, `notary_*` | `app_store_connect` |
| In-app purchase transactions, subscription status, refunds, server notifications | `storekit_*`, `apple_jws_decode` | `app_store_server` |
| Apple Music catalog/charts/library/playlists | `music_*` | `music` |
| Weather forecasts/alerts | `weather`, `weather_*` | `weatherkit` |
| Geocoding, place search, ETAs, directions | `maps_*` | `maps` |
| CloudKit public database records | `cloudkit_*` | `cloudkit` |
| Send a push notification | `apns_send` | `apns` |
| Sign in with Apple secrets/tokens | `siwa_*` | `sign_in_with_apple` |
| Device enrollment / MDM assignment (ABM, ASM) | `abm_*` | `business_manager` |
| Search Ads campaigns & reports | `searchads_*` | `search_ads` |
| Apple News Publisher channel/articles | `news_*` | `news` |
| iCloud Calendar, Contacts, Mail | `icloud_calendars`, `icloud_events`, `icloud_event_*`, `icloud_contacts`, `icloud_contact_*`, `icloud_mail_*` | `icloud` (app-specific password) |
| iCloud Reminders, Notes, Drive, Photos, Find My, Hide My Email | `icloud_login`, `icloud_2fa`, `icloud_reminders*`, `icloud_notes*`, `icloud_drive_*`, `icloud_photos_*`, `icloud_findmy_*`, `icloud_hme_*` (see the `icloud` skill) | `icloud.password` (Apple ID password, 2FA once) |

Every authenticated family has a generic `*_request` tool for endpoints without a dedicated
tool; find endpoints with `apple_docs_index` (technologies: `appstoreconnectapi`,
`appstoreserverapi`, `applemusicapi`, `weatherkitrestapi`, `applemapsserverapi`,
`cloudkitwebservices`, `usernotifications`, `signinwithapplerestapi`, `applebusinessmanagerapi`,
`apple_search_ads`, `applenewsapi`).

## Conventions

- Responses are JSON trimmed to ~50k characters. If truncated, narrow with `limit`, `fields`,
  filters, or `compact`. JSON:API `links` are stripped by default (`compact: true`).
- Errors come back as tool errors with the HTTP status, Apple's message and a hint
  (401 → key/permissions; 403 → role or agreement; 429 → rate limit).
- Tokens are cached in memory (ASC 15 min, Music 11 h, WeatherKit 50 min, Maps 25 min, APNs 45 min,
  OAuth per `expires_in`). After editing the config file call `apple_reload_config`.
- Destructive tools (delete tester, delete event/contact, move mail, CloudKit modify, revoke) are
  annotated `destructiveHint`; confirm with the user before running them on real data.
- Dates: tools accept RFC 3339 or `YYYY-MM-DD`; StoreKit millisecond timestamps get `*_iso` twins.

## Deeper references (load the matching skill)

`app-store-connect`, `app-store-server`, `apple-music`, `weatherkit`, `apple-maps`, `cloudkit`,
`apns`, `sign-in-with-apple`, `apple-business-manager`, `apple-search-ads`, `apple-news`, `icloud`.
