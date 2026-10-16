# iCloud web sidecar (optional)

A small Python MCP server that covers iCloud services **without an official API** —
Find My, iCloud Drive, Photos, Reminders, Notes, Hide My Email — through the
community library [pyicloud](https://pypi.org/project/pyicloud/).

It is **not** started by default: the main `apple` server is Rust and needs no Python.
Register it only if you want these services:

```bash
uv sync --project "<plugin root>/sidecar/icloud-web"
claude mcp add --scope user apple-icloud-web -- uv run --project "<plugin root>/sidecar/icloud-web" apple-icloud-web-mcp
```

Configuration (same file as the main server, `~/.config/apple-mcp/config.json`):

```json
{ "icloud": { "apple_id": "you@icloud.com", "password": "your full Apple ID password" } }
```

First use: call `icloud_login`, approve the 2FA prompt on an Apple device and submit the
code with `icloud_2fa`. The trusted session is cached under `~/.local/share/apple-mcp/icloud-session`.

Caveats: private Apple web APIs, may break at any time; Apple may ask for 2FA again
periodically; never share the config file (it holds your real password).
