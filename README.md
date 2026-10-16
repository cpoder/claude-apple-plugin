# Apple plugin for Claude Code

Claude Code plugin that gives Claude access to Apple's **online** services through a single
MCP server written in **Rust** ([rmcp](https://crates.io/crates/rmcp)), plus skills that teach
Claude each API, and an optional Python sidecar for the iCloud services that have no official API.

| Family | Tools | What you can do |
| :- | :- | :- |
| Public (no credentials) | `itunes_search`, `itunes_lookup`, `apple_charts`, `apple_system_status`, `apple_docs`, `apple_docs_index`, `apple_technologies`, `apple_news` | App Store / iTunes catalog, top charts, Apple outages, developer documentation rendered as Markdown, Apple & developer news |
| App Store Connect | `asc_*` (18), `notary_*` (2) | apps, builds, TestFlight groups/testers, App Store versions, customer reviews & replies, signing assets, users, sales/finance reports, Xcode Cloud, notarization status |
| App Store Server | `storekit_*` (8), `apple_jws_decode` | in-app purchase transactions, subscription status, order lookup, refunds, server notifications (JWS decoded) |
| Apple Music | `music_*` (8) | catalog search/lookup/charts, user library, playlists |
| WeatherKit | `weather`, `weather_*` (3) | current conditions, hourly/daily forecasts, next-hour precipitation, alerts |
| Apple Maps Server | `maps_*` (8) | geocoding, place search, autocomplete, ETAs, directions, place details |
| CloudKit Web Services | `cloudkit_*` (5) | query / lookup / modify records in a container's public database |
| APNs | `apns_send` | push notifications (alert, background, Live Activity, VoIP…) over HTTP/2 |
| Sign in with Apple | `siwa_*` (5) | client secret, identity-token verification, code exchange, refresh, revoke |
| Business / School Manager | `abm_*` (6) | device inventory, MDM servers, device assignment |
| Search Ads | `searchads_*` (4) | organizations, campaigns, performance reports |
| Apple News Publisher | `news_*` (6) | channel, sections, articles, publish/update |
| iCloud | `icloud_calendars`, `icloud_events`, `icloud_event_*`, `icloud_contacts`, `icloud_contact_*`, `icloud_mail_*` | Calendar (CalDAV), Contacts (CardDAV), Mail (IMAP/SMTP) |
| iCloud web (optional sidecar) | `icloud_findmy_*`, `icloud_drive_*`, `icloud_photos_*`, `icloud_reminders*`, `icloud_notes*`, `icloud_hme_*` | Find My, iCloud Drive, Photos, Reminders, Notes, Hide My Email via pyicloud |

Skills: `/apple:setup`, `/apple:status`, `/apple:docs <query>`, plus reference skills Claude
loads on demand (`app-store-connect`, `app-store-server`, `apple-music`, `weatherkit`,
`apple-maps`, `cloudkit`, `apns`, `sign-in-with-apple`, `apple-business-manager`,
`apple-search-ads`, `apple-news`, `icloud`). Agent: `apple:apple-expert`.

## Requirements

- Claude Code ≥ 2.1.281
- Rust toolchain (`cargo`, https://rustup.rs) to build the server once (Linux, macOS, WSL)
- Optional: `uv` + Python ≥ 3.10 for the iCloud web sidecar

## Install

From a local checkout (loads in place, so edits are picked up on restart):

```bash
claude plugin marketplace add /path/to/claude-apple-plugin
claude plugin install apple@cpo-plugins
```

Or for development only: `claude --plugin-dir /path/to/claude-apple-plugin`.

Build the server once (otherwise the first MCP connection builds it in the background, fails
once with an explanatory message, and works after `/mcp` → reconnect):

```bash
bash /path/to/claude-apple-plugin/scripts/apple-mcp build
```

The binary is built into `~/.local/share/apple-mcp` (or `$CLAUDE_PLUGIN_DATA` when Claude Code
launches the server; a binary built manually is reused from there) and rebuilt automatically
when the sources change. `/apple:setup` runs this build for you.

## Configure

Run `/apple:setup` inside Claude Code. It drives the automated setup built into the server:

```bash
bash scripts/apple-mcp setup            # interactive wizard in a terminal
bash scripts/apple-mcp setup auto       # non-interactive: discover, probe, write, report
bash scripts/apple-mcp setup auto --json --want music,maps team_id=ABCDE12345
```

Apple forces exactly one manual step per key: click "generate" in a web console and download
the key once. `setup auto` does the rest:

- **finds** `AuthKey_*.p8` / `SubscriptionKey_*.p8` / `.pem` files in `~/.config/apple-mcp`,
  your home, `Downloads`/`Desktop`/`Documents` (including the Windows profile under WSL) and the
  current directory, imports them into `~/.config/apple-mcp/keys/` (mode 600) and reads the key
  id from the file name;
- **probes** every key against every API it could be enabled for (App Store Connect team or
  individual key, App Store Server, Music, Maps, WeatherKit, APNs, Sign in with Apple, CloudKit,
  Business Manager, Search Ads, News, iCloud) and keeps what works;
- **derives** the Team ID and the apps' bundle ids from App Store Connect, the Search Ads org id,
  the issuer id shared by App Store Connect and In-App Purchase keys, the Music storefront;
- **generates** the EC keys CloudKit and Search Ads need and prints the public key to paste;
- **writes** `~/.config/apple-mcp/config.json` (mode 600) and lists what is still missing, with
  the exact URL to click for each item (`setup links`, `setup open <portal>` opens it in your
  browser, also from WSL).

Values can be passed as `section.key=value`, as a JSON object on stdin (`--stdin`, for
secrets and pasted PEM keys), or through Claude: paste the key content or its path in the chat.
Each section is independent: configure only the services you use. Environment variables
override the file (`APPLE_MCP_<SECTION>_<KEY>`, e.g. `APPLE_MCP_APP_STORE_CONNECT_KEY_ID`;
`APPLE_TEAM_ID`; `APPLE_MCP_CONFIG` to point at another file). The server reloads the file when
it changes. Check with `bash scripts/apple-mcp check` (what is configured),
`bash scripts/apple-mcp doctor` (one read-only call per configured service) or `/apple:status`.

| Section | Credentials | Where |
| :- | :- | :- |
| `app_store_connect` | API key (Key ID, Issuer ID, `.p8`) | App Store Connect → Users and Access → Integrations → App Store Connect API |
| `app_store_server` | In-App Purchase key + bundle id | same page → In-App Purchase |
| `music` | key with Media Services (MusicKit) (+ optional Music User Token) | developer.apple.com → Keys |
| `weatherkit` | key with WeatherKit + Services ID | developer.apple.com → Keys / Identifiers → Services IDs |
| `maps` | key with Maps | developer.apple.com → Keys |
| `cloudkit` | server-to-server key (generated locally, public key pasted) + container | CloudKit Console → Tokens & Keys |
| `apns` | key with APNs + bundle id topic | developer.apple.com → Keys |
| `sign_in_with_apple` | key with Sign in with Apple + Services ID / bundle id | developer.apple.com → Keys |
| `business_manager` | API client (client id, key id, `.pem`) | Apple Business Manager → Preferences → API |
| `search_ads` | API client (generated locally, public key uploaded) + org id | Apple Search Ads → Account Settings → API |
| `news` | channel ID, key ID, key secret | News Publisher |
| `icloud` | Apple ID e-mail + app-specific password (+ full password for the sidecar) | account.apple.com → App-Specific Passwords |

## Layout

```
.claude-plugin/plugin.json     manifest (+ marketplace.json for local install)
.mcp.json                      starts scripts/apple-mcp (Rust server) with CLAUDE_PLUGIN_ROOT/DATA
scripts/apple-mcp              launcher: builds with cargo into the plugin data dir, then execs the binary
server/                        Rust crate `apple-mcp` (rmcp 3, reqwest, p256 ES256 JWTs, roxmltree, imap, lettre)
  src/tools/*.rs               one module per Apple service (each adds a tool router)
skills/*/SKILL.md              /apple:setup, /apple:status, /apple:docs + API reference skills
agents/apple-expert.md         subagent for multi-step Apple tasks
sidecar/icloud-web/            optional Python MCP server (pyicloud) for Drive/Photos/Find My/Reminders/Notes
config.example.json            every config section with where to get each credential
```

## Development

```bash
cd server
cargo test                  # unit tests (JWT, CloudKit signature, ICS/vCard/WebDAV parsing, docs rendering…)
cargo run -- --check        # config status
cargo run -- doctor         # one read-only call per configured service
cargo run -- setup auto     # automated credential setup (setup --help)
cargo run                   # MCP server on stdio
```

Add a service: create `src/tools/<name>.rs` with a `#[tool_router(router = <name>_router, vis = "pub")]`
impl block, register it in `src/tools/mod.rs` and in `AppleServer::new()` (`server.rs`), add a
`ServiceSpec` in `config.rs`, and a skill under `skills/`.

## Notes

- Responses are JSON, trimmed to ~50k characters; use limits/fields/filters for big resources.
- Tool errors carry Apple's HTTP status + message + a hint (401 key/clock, 403 role, 429 rate limit).
- The server never prints secrets; keep `config.json` and `.p8` keys at mode 600.
- iTunes Search API is rate limited (~20 calls/min). WeatherKit includes 500k calls/month, Maps 250k/day.
- The iCloud web sidecar uses Apple's private web APIs via pyicloud and can break without notice.

## Démarrage rapide (FR)

1. `claude plugin marketplace add /chemin/vers/claude-apple-plugin` puis `claude plugin install apple@cpo-plugins`.
2. `bash scripts/apple-mcp build` (compile le serveur Rust une fois, quelques minutes).
3. Dans Claude Code : `/apple:setup` (ou `bash scripts/apple-mcp setup` dans un terminal) :
   il trouve les clés téléchargées, les teste contre chaque API Apple, déduit le Team ID et les
   bundle ids, écrit le config et affiche le lien exact à ouvrir pour ce qui manque. Collez la
   clé obtenue (ou laissez-la dans Téléchargements) et relancez.
4. `/apple:status` pour vérifier ; les outils publics (recherche App Store, classements, état des
   services Apple, documentation développeur) fonctionnent sans aucun identifiant.

License: MIT.
