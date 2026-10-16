---
name: apple-business-manager
description: Apple Business Manager / Apple School Manager API reference for the apple plugin — list organization devices, MDM servers, assign or unassign devices to an MDM, poll activities. Load for device enrollment, fleet inventory or abm_* tools.
user-invocable: false
---

# Apple Business Manager & School Manager API (tools `abm_*`)

Auth: OAuth 2 client-credentials with a JWT client assertion (ES256, `sub`/`iss` = client id,
`aud` https://account.apple.com/auth/oauth2/v2/token, `kid` = key id) exchanged at
`https://account.apple.com/auth/oauth2/token` with scope `business.api` (ABM, host
`api-business.apple.com`) or `school.api` (ASM, host `api-school.apple.com`). Config
`business_manager.client_id/key_id/private_key/scope`. Setup: ABM → Preferences → API → Add
(the `.pem` private key is downloadable once; an admin/account-manager role is needed).

## Tools

- `abm_devices limit=1000 max_pages=5` → `orgDevices` with `serialNumber`, `deviceModel`,
  `productFamily` (iPhone, iPad, Mac, Apple TV, Apple Watch, Apple Vision Pro), `productType`,
  `status` (ASSIGNED, UNASSIGNED), `purchaseSourceType` (APPLE, RESELLER, MANUALLY_ADDED),
  `purchaseSourceUuid`, `orderNumber`, `orderDateTime`, `addedToOrgDateTime`, `updatedDateTime`,
  `deviceCapacity`, `color`, `partNumber`, `imei`, `meid`, `eid`, `wifiMacAddress`, `bluetoothMacAddress`.
  Filter by fields with `abm_request GET orgDevices query={"fields[orgDevices]":"serialNumber,status"}`.
- `abm_device device_id` → device + assigned MDM server. (The id is Apple's device id, not the serial;
  find it by scanning `abm_devices` or `abm_request GET orgDevices?filter[serialNumber]=…` where supported.)
- `abm_mdm_servers include_devices=true` → `mdmServers` (`serverName`, `serverType` MDM|APPLE_CONFIGURATOR,
  created/updated) with assigned device ids.
- `abm_assign_devices device_ids=[…] mdm_server_id=… activity_type=ASSIGN_DEVICES|UNASSIGN_DEVICES`
  → `orgDeviceActivities` id; `abm_activity activity_id` → `status` (PENDING, IN_PROGRESS, COMPLETED,
  STOPPED, COMPLETED_WITH_ERRORS, FAILED), `subStatus`, counts, `downloadUrl` (CSV result log).

Pagination uses `links.next` (`max_pages`). Rate limits apply per client; batch assignments in one
activity rather than one call per device. Docs: `apple_docs_index technology=applebusinessmanagerapi`
(and `appleschoolmanagerapi`).
