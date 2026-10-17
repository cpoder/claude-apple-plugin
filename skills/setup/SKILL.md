---
name: setup
description: Configure credentials for the Apple plugin (App Store Connect, App Store Server, Apple Music, WeatherKit, Maps, CloudKit, APNs, Sign in with Apple, Business Manager, Search Ads, News, iCloud) and build/check the Rust MCP server. Use when the user wants to set up, connect, or add an Apple service.
disable-model-invocation: true
argument-hint: "[service]"
allowed-tools: Bash(bash *) Bash(ls *) Bash(cat *) Bash(mkdir *) Bash(chmod *) Bash(test *) Bash(APPLE_MCP_DATA_DIR=*) Read Write Edit
---

# /apple-services:setup — connect Apple services

Requested service (may be empty): `$ARGUMENTS`

Talk to the user in their language. The setup is automated by the server binary: **you never
write the JSON config by hand and you never ask for something the report does not list.**
Apple only forces one manual step per key: clicking "generate" in a web console and
downloading the key once. Everything else (finding the key file, reading its id, testing it
against every Apple API, deriving the Team ID and bundle ids, writing the config) is done by:

```bash
APPLE_MCP_DATA_DIR="${CLAUDE_PLUGIN_DATA}" bash "${CLAUDE_PLUGIN_ROOT}/scripts/apple-mcp" setup auto --json [--want a,b] [section.key=value …]
```

The first run builds the Rust binary (needs `cargo`; several minutes). If cargo is missing,
tell the user to install Rust from https://rustup.rs and stop.

Section names: `app_store_connect`, `app_store_server`, `music`, `weatherkit`, `maps`,
`cloudkit`, `apns`, `sign_in_with_apple`, `business_manager`, `search_ads`, `news`, `icloud`.

## 1. Which services

If `$ARGUMENTS` names a service, map it to its section and use `--want <section>`.
Otherwise ask once (AskUserQuestion, several allowed) which of these the user wants:
App Store Connect · App Store Server (in-app purchases) · Apple Music · WeatherKit ·
Apple Maps · CloudKit · APNs push · Sign in with Apple · Business/School Manager ·
Search Ads · Apple News Publisher · iCloud (Calendar/Contacts/Mail) · iCloud web extras
(Reminders/Notes/Drive/Photos/Find My/Hide My Email, see §5). Then run `setup auto --json --want <sections>`.

## 2. Read the report, show links, collect only what is missing

The JSON report contains:

- `keys[]`: key files found (also in `~/Downloads`, `~/Desktop`, and the Windows profile under
  WSL) and imported into `~/.config/apple-mcp/keys/`, with `key_id` and `used_by`.
- `team_id`, `apps[]` (name + bundle id) derived from App Store Connect when it works.
- `services[]`: `status` is `ok`, `needs` or `skipped`. For `needs`, each entry of `needs[]`
  has `field` (`section.key`), `how` (what to do) and `url` (the exact page to click).
  `choices` lists candidate values (e.g. several apps for a bundle id).
- `public_keys`: PEM public keys to paste into the CloudKit / Search Ads consoles.
- `changed`, `warnings`.

Present it like this:

1. One line per service: ✅ configured and verified / ⏳ needs … / ⏭ skipped.
2. For the services that need something, **group by `url`** so the user opens each Apple page
   once. Show the URL as a clickable link, then the `how` text for each field. Offer to open the
   page for them: `… setup open <portal>` (works from Linux, macOS and WSL).
3. Tell the user the three ways to hand a key over, all equivalent:
   - download `AuthKey_<ID>.p8` / `SubscriptionKey_<ID>.p8` and **leave it in Downloads**
     (Windows or Linux): the next run picks it up;
   - **paste the content** of the `.p8` file into the chat;
   - paste the **path** of the file.
   For ids (issuer id, service id, client id, key id, app-specific password…) they paste the value.
4. For a field with `choices`, use AskUserQuestion with those choices.
5. Show `public_keys.<section>` in a code block only for CloudKit / Search Ads when they are
   wanted and not yet `ok` ("paste this in the console, then give me the Key ID").

Do not list portals or fields for services the user did not choose. Do not ask for a Team ID
if App Store Connect is among the wanted services and not yet configured: it is derived from it.

## 3. Apply the answers and re-run

Pass values on the command line (`team_id=…`, `weatherkit.service_id=…`); the config is
merged, never replaced. For secrets and PEM content use `--stdin` with a JSON object so the
secret never appears in a command line:

```bash
APPLE_MCP_DATA_DIR="${CLAUDE_PLUGIN_DATA}" bash "${CLAUDE_PLUGIN_ROOT}/scripts/apple-mcp" setup auto --json --want music,maps --stdin <<'EOF'
{"team_id": "ABCDE12345", "music": {"private_key_path": "-----BEGIN PRIVATE KEY-----\n…\n-----END PRIVATE KEY-----"}}
EOF
```

A PEM given as any field value is saved as a key file (mode 600) and `private_key_path` is set.
A file path the user gives: `--key <path>` (it is imported). Every run re-probes everything
and rewrites the config, so there is no separate "verify" step. Repeat §2–§3 until every
wanted service is `ok`, or the user wants to stop (what is done stays configured).

Never echo a secret or a private key back; refer to key files by name.

## 4. Final check

- If the `apple` MCP server is connected, call `apple_test_service` (no reconnect needed: the
  server reloads the config file when it changes) and report its table.
- If it is not connected (first install: the launcher built in the background and refused the
  first connection), tell the user to run `/mcp` and reconnect `apple`, or restart Claude Code.
  `… scripts/apple-mcp doctor` gives the same verification from the shell meanwhile.
- WeatherKit can take ~30 minutes to activate after enabling it on the key / Services ID: a 401
  right after creation is expected; say so.

## 5. iCloud web extras (Reminders, Notes, Drive, Photos, Find My, Hide My Email)

Built into the Rust server. They need the full Apple ID password in `icloud.password` (warn: it is
the real account password, stored in the 600-mode config file, used against Apple's private
icloud.com APIs) and one two-factor code. Only if the user explicitly wants these:

1. Save the password with `--stdin`: `{"icloud": {"apple_id": "…", "password": "…"}}`.
2. Run `… scripts/apple-mcp icloud login`. If it reports `two_factor_required`, Apple pushed a
   6-digit code to the user's devices: ask for it and run `… icloud 2fa <code>` right away (codes
   expire). No prompt on the devices → `… icloud sms` sends it by SMS, then `icloud 2fa <code>`.
   If Apple answers that updated iCloud terms must be accepted, the user signs in once at
   https://www.icloud.com/ and accepts them, then `… icloud login` again (no 2FA this time).
3. Prove it with `… icloud lists` (Reminders lists). The session is cached; later logins are silent.

## Reference: other setup commands

| Command | Use |
| :- | :- |
| `setup links [portal]` | print the Apple URLs (`asc-keys`, `iap-keys`, `dev-keys`, `services-ids`, `cloudkit`, `abm`, `searchads`, `news`, `icloud-passwords`, `membership`, `vendor`) |
| `setup open <portal>` | open that page in the user's browser |
| `setup discover` | list / import key files without probing |
| `setup pubkey cloudkit\|search_ads` | print the public key to paste (generated once) |
| `setup set k=v … \| --stdin` | write values without probing |
| `setup wizard` | interactive terminal version (for a human in a shell: `! bash "${CLAUDE_PLUGIN_ROOT}/scripts/apple-mcp" setup` is not interactive; they should run it in a separate terminal) |
