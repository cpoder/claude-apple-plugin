---
name: cloudkit
description: CloudKit Web Services reference for the apple plugin — query, look up, create, update and delete records in a CloudKit container's public database with a server-to-server key; zones; field value formats; filters and sorts. Load for CloudKit data inspection/maintenance or cloudkit_* tools.
user-invocable: false
---

# CloudKit Web Services (tools `cloudkit_*`)

Auth: server-to-server key (EC P-256) → each request is signed:
`date:base64(sha256(body)):path` with ECDSA/SHA-256, headers
`X-Apple-CloudKit-Request-KeyID`, `-ISO8601Date`, `-SignatureV1`. Handled by the server from
`cloudkit.container/key_id/private_key`, `environment` (development|production), `database`
(server keys can only use the **public** database). Base:
`https://api.apple-cloudkit.com/database/1/{container}/{env}/public/...`.

## Records

- Query: `cloudkit_query record_type=Post filter_by=[{"comparator":"EQUALS","fieldName":"status","fieldValue":{"value":"published"}}] sort_by=[{"fieldName":"createdAt","ascending":false}] results_limit=50 desired_keys=[…]`
  → `records[]` with `recordName`, `recordType`, `recordChangeTag`, `fields.{name:{value,type}}`,
  `created/modified.{timestamp,userRecordName}`; paginate with `continuationMarker`.
  Comparators: EQUALS, NOT_EQUALS, LESS_THAN(_OR_EQUALS), GREATER_THAN(_OR_EQUALS), NEAR (location
  with `distance`), CONTAINS_ALL_TOKENS, CONTAINS_ANY_TOKENS (searchable string fields), IN, NOT_IN,
  LIST_CONTAINS(_ALL|_ANY), BEGINS_WITH. Queried fields need the "Queryable"/"Sortable" index in
  the CloudKit schema (system `recordName` always works: `fieldName: "___recordID"`).
- Lookup by name: `cloudkit_lookup record_names=[…]`.
- Modify: `cloudkit_modify operations=[{"operationType":"create","record":{"recordType":"Post","fields":{"title":{"value":"Hi"},"views":{"value":3},"when":{"value":1700000000000},"tags":{"value":["a","b"]},"author":{"value":{"recordName":"u1","action":"NONE"}},"where":{"value":{"latitude":48.8,"longitude":2.3}}}}}]`
  — update/forceUpdate/replace/forceReplace/delete/forceDelete; non-force ops require the current
  `recordChangeTag`. Field values: strings, numbers, booleans → number 0/1 (type INT64 in schema), dates =
  ms since epoch (type TIMESTAMP), lists, references `{recordName, action}` (NONE|DELETE_SELF|VALIDATE),
  locations, assets (`assets/upload` two-step).
- Zones: `cloudkit_zones`. Public database only has `_defaultZone` for most apps.
- Other operations via `cloudkit_request subpath=records/changes|zones/lookup|assets/upload|users/caller`.

Limits: 200 records per query page; request body ≤ 1 MB; the public database has per-container
quotas that grow with the user base. Errors come as `serverErrorCode` (AUTHENTICATION_FAILED → key
not uploaded/activated for this environment, BAD_REQUEST → unindexed field, NOT_FOUND).
Docs: `apple_docs_index technology=cloudkitwebservices`.
