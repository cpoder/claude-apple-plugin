---
name: status
description: Show which Apple services are configured in the apple plugin and whether Apple's consumer/developer systems currently have outages. Use when the user asks "is Apple down", "what Apple services are connected", or to troubleshoot the apple MCP server.
argument-hint: "[developer|consumer]"
allowed-tools: Bash(bash *) Bash(APPLE_MCP_DATA_DIR=*)
---

# /apple-services:status

1. Call `apple_status` (MCP), then `apple_test_service` to prove each configured service with
   one read-only call. If the tools are unavailable, the server is not connected: run
   `APPLE_MCP_DATA_DIR="${CLAUDE_PLUGIN_DATA}" bash "${CLAUDE_PLUGIN_ROOT}/scripts/apple-mcp" doctor` (same checks from the shell) and suggest
   `/mcp` → reconnect `plugin:apple-services:apple`, or `/apple-services:setup` for anything missing or failing.
2. Call `apple_system_status` with scope `developer` and scope `consumer` (or only the scope in
   `$ARGUMENTS`).
3. Summarize in a short table: configured services (verified / failing with Apple's error, and
   the missing keys for the others) and any current Apple incident (service, status, message,
   since when). Mention the config path and that `/apple-services:setup` fixes what fails.
