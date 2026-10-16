# Apple plugin for Claude Code

A Claude Code plugin that gives Claude access to Apple's **online** services through a single
MCP server written in **Rust** ([rmcp](https://crates.io/crates/rmcp)), plus skills that teach
Claude each API and an agent for multi-step tasks. 125 tools, one binary, no Node or Python,
runs on Linux, macOS and WSL. The iCloud services that have no official API (Reminders, Notes,
Drive, Photos, Find My, Hide My Email) are reached through the same server via the icloud.com
web session.

## What it covers

| Family | Tools | What you can do |
| :- | :- | :- |
| Public (no credentials) | `itunes_search`, `itunes_lookup`, `apple_charts`, `apple_system_status`, `apple_docs`, `apple_docs_index`, `apple_technologies`, `apple_news` | App Store / iTunes catalog, top charts, Apple outages, developer documentation rendered as Markdown, Apple and developer news |
| Server | `apple_status`, `apple_configure`, `apple_test_service`, `apple_config_template`, `apple_reload_config` | see what is configured, write credentials safely, verify every service with a read-only call |
| App Store Connect | `asc_*` (16), `notary_*` (2) | apps, builds, TestFlight groups and testers, App Store versions, customer reviews and replies, signing assets, users, sales and finance reports, Xcode Cloud, notarization status |
| App Store Server | `storekit_*` (8), `apple_jws_decode` | in-app purchase transactions, subscription status, order lookup, refunds, server notifications (JWS decoded) |
| Apple Music | `music_*` (8) | catalog search, lookup, charts, user library, playlists |
| WeatherKit | `weather`, `weather_*` (3) | current conditions, hourly and daily forecasts, next-hour precipitation, alerts |
| Apple Maps Server | `maps_*` (8) | geocoding, place search, autocomplete, ETAs, directions, place details |
| CloudKit Web Services | `cloudkit_*` (5) | query, look up and modify records in a container's public database |
| APNs | `apns_send` | push notifications (alert, background, Live Activity, VoIP...) over HTTP/2 |
| Sign in with Apple | `siwa_*` (5) | client secret, identity-token verification, code exchange, refresh, revoke |
| Business / School Manager | `abm_*` (6) | device inventory, MDM servers, device assignment |
| Search Ads | `searchads_*` (4) | organizations, campaigns, performance reports |
| Apple News Publisher | `news_*` (6) | channel, sections, articles, publish and update |
| iCloud (app-specific password) | `icloud_calendars`, `icloud_events`, `icloud_event_*`, `icloud_contacts`, `icloud_contact_*`, `icloud_mail_*` | Calendar (CalDAV), Contacts (CardDAV), Mail (IMAP/SMTP) |
| iCloud web (Apple ID password + 2FA once) | `icloud_login`, `icloud_2fa`, `icloud_account`, `icloud_reminders*`, `icloud_reminder_*`, `icloud_notes*`, `icloud_note`, `icloud_drive_*`, `icloud_photos_*`, `icloud_findmy_*`, `icloud_hme_*` | Reminders, Notes, iCloud Drive, Photos, Find My, Hide My Email through the private icloud.com APIs (SRP login, trusted session cached) |

Every authenticated family also has a generic `*_request` tool for endpoints without a dedicated
tool. Skills: `/apple:setup`, `/apple:status`, `/apple:docs <query>`, plus reference skills Claude
loads on demand (`app-store-connect`, `app-store-server`, `apple-music`, `weatherkit`,
`apple-maps`, `cloudkit`, `apns`, `sign-in-with-apple`, `apple-business-manager`,
`apple-search-ads`, `apple-news`, `icloud`). Agent: `apple:apple-expert`.

## Quick start

1. Install the plugin (see below). The first connection downloads the server binary.
2. In Claude Code run `/apple:setup`, or in a terminal `bash scripts/apple-mcp setup`. It finds the
   keys you downloaded, probes them against every Apple API, derives your Team ID and bundle ids,
   writes the config and prints the exact link to open for anything still missing.
3. Check with `/apple:status`. The public tools (App Store search, charts, Apple system status,
   developer documentation) work without any credentials.

## Requirements

- Claude Code 2.1.281 or later
- Linux (x86_64, arm64), macOS (Apple Silicon, Intel) or WSL: the launcher downloads a prebuilt,
  checksum-verified server binary from the GitHub release matching the plugin version. Nothing to
  compile. A Rust toolchain (https://rustup.rs) is only needed on other platforms or to build from
  source.

## Install

From GitHub:

```bash
claude plugin marketplace add cpoder/claude-apple-plugin
claude plugin install apple@cpo-plugins
```

From a local checkout (loads in place, so edits are picked up on restart):

```bash
claude plugin marketplace add /path/to/claude-apple-plugin
claude plugin install apple@cpo-plugins
```

For development only: `claude --plugin-dir /path/to/claude-apple-plugin`.

### How the server binary gets there

`scripts/apple-mcp` (started by Claude Code through `.mcp.json`) looks for a binary in the plugin
data directory, then downloads `apple-mcp-v<version>-<target>.tar.gz` from the matching GitHub
release and verifies it against `SHA256SUMS.txt`. Only when no asset exists for the platform (or
there is no network) does it fall back to `cargo build`, in the background, and the connection
succeeds on the next `/mcp` → reconnect.

```bash
bash scripts/apple-mcp download   # fetch the release binary now (default behaviour)
bash scripts/apple-mcp build      # build from source instead, and keep rebuilding when sources change
```

`APPLE_MCP_BUILD=download` forbids compiling, `APPLE_MCP_BUILD=source` forbids downloading,
`APPLE_MCP_REPO=owner/repo` points at a fork's releases.

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
secrets and pasted PEM keys), or through Claude: paste the key content or its path in the chat
and Claude calls `apple_configure`. Each section is independent: configure only the services you
use. Environment variables override the file (`APPLE_MCP_<SECTION>_<KEY>`, e.g.
`APPLE_MCP_APP_STORE_CONNECT_KEY_ID`; `APPLE_TEAM_ID`; `APPLE_MCP_CONFIG` to point at another
file). The server reloads the file whenever it changes on disk. Check with
`bash scripts/apple-mcp check` (what is configured), `bash scripts/apple-mcp doctor` (one
read-only call per configured service), `apple_test_service` from Claude, or `/apple:status`.

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
| `icloud` | Apple ID e-mail + app-specific password (Calendar/Contacts/Mail); + the full Apple ID `password` for the iCloud web tools (Reminders, Notes, Drive, Photos, Find My, Hide My Email; one 2FA code at first login) | account.apple.com → App-Specific Passwords |

## Layout

```
.claude-plugin/plugin.json     manifest (+ marketplace.json for local install)
.mcp.json                      starts scripts/apple-mcp (Rust server) with CLAUDE_PLUGIN_ROOT/DATA
scripts/apple-mcp              launcher: builds with cargo into the plugin data dir, then execs the binary
server/                        Rust crate `apple-mcp` (rmcp 3, reqwest, p256 ES256 JWTs, roxmltree, imap, lettre)
  src/tools/*.rs               one module per Apple service (each adds a tool router)
  src/tools/icloud_web/        icloud.com web session (SRP + 2FA, cookies) and Reminders/Notes/Drive/Photos/Find My/Hide My Email
  src/tools/doctor.rs          one read-only probe per service (apple_test_service, `apple-mcp doctor`)
  src/setup.rs                 `apple-mcp setup`: credential discovery, probing, config writing
skills/*/SKILL.md              /apple:setup, /apple:status, /apple:docs + API reference skills
agents/apple-expert.md         subagent for multi-step Apple tasks
config.example.json            every config section with where to get each credential
```

## Development

```bash
cd server
cargo test                  # unit tests (JWT, CloudKit signature, SRP, ICS/vCard/WebDAV parsing, docs rendering...)
cargo run -- --check        # config status
cargo run -- doctor         # one read-only call per configured service
cargo run -- setup auto     # automated credential setup (setup --help)
cargo run -- icloud login   # iCloud web session from a shell (icloud 2fa <code> | sms | lists | reminders <list>)
cargo run                   # MCP server on stdio
```

Add a service: create `src/tools/<name>.rs` with a `#[tool_router(router = <name>_router, vis = "pub")]`
impl block, register it in `src/tools/mod.rs` and in `AppleServer::new()` (`server.rs`), add a
`ServiceSpec` in `config.rs`, a probe in `tools/doctor.rs`, and a skill under `skills/`.

### Releases

CI (`.github/workflows/ci.yml`) runs `cargo test`, `cargo clippy -D warnings`, ShellCheck on the
launcher and `claude plugin validate` on every push and pull request. Publishing a release
(`.github/workflows/release.yml`): bump `version` in `server/Cargo.toml`, `.claude-plugin/plugin.json`
and `.claude-plugin/marketplace.json`, then push a tag `vX.Y.Z` with the same number. The workflow
builds Linux x86_64/arm64, macOS arm64/x86_64 and Windows x86_64 binaries and attaches them with
`SHA256SUMS.txt` to the GitHub release that the launcher downloads.

## Compared to other Apple MCP servers

- Breadth: most projects cover one service (App Store Connect, iCloud mail/calendar, Apple Music,
  Notes). This plugin covers twelve authenticated families plus the public endpoints, and ships
  skills and an agent for Claude Code.
- Portability: one Rust binary, no AppleScript, so it works headless on Linux and WSL.
- Setup: `setup auto` discovers and probes keys instead of asking you to fill a config by hand.
- Depth: App Store Connect-only servers generated from Apple's OpenAPI spec expose far more
  endpoints (hundreds of tools); here the common workflows are dedicated tools and everything else
  goes through `asc_request`.

## Notes

- Responses are JSON, trimmed to about 50k characters; use limits, fields and filters for big resources.
- Tool errors carry Apple's HTTP status and message plus a hint (401 key/clock, 403 role, 429 rate limit).
- The server never prints secrets; `config.json` and key files are written with mode 600.
- Signed StoreKit payloads are decoded, not signature-verified; verify the x5c chain yourself before trusting a payload received from a third party.
- iTunes Search API is rate limited (about 20 calls/min). WeatherKit includes 500k calls/month, Maps 250k/day.
- The iCloud web tools use Apple's private icloud.com APIs (the same calls as the web app) and can break without notice. The session (cookies + trust token) is cached in the plugin data dir so the 2FA code is asked once.
- Version 0.1.0: the public tools and the iCloud web login path are verified live; the other authenticated families follow Apple's documentation but have had less real-world testing. Issues and pull requests are welcome.

License: MIT.
