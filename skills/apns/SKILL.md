---
name: apns
description: Apple Push Notification service reference for the apple plugin — send alert, background, live activity, VoIP or complication pushes with token-based auth, payload keys, priorities, sandbox vs production and APNs error reasons. Load when sending or debugging push notifications (apns_send).
user-invocable: false
---

# APNs (tool `apns_send`)

Token auth: ES256 JWT (`iss` team id, `kid` APNs key id, `iat`), refreshed between 20 and 60
minutes — the server caches it 45 min. The same key works for every app of the team. Hosts:
production `api.push.apple.com`, sandbox `api.sandbox.push.apple.com` (development builds /
Xcode runs). A device token is valid for exactly one environment and one app.

## Payload cheat-sheet (`payload` argument)

```json
{"aps": {"alert": {"title": "Title", "subtitle": "Sub", "body": "Body", "launch-image": ""},
          "badge": 1, "sound": "default", "thread-id": "chat-1", "category": "REPLY",
          "mutable-content": 1, "content-available": 1, "interruption-level": "time-sensitive", "relevance-score": 0.8},
 "custom": {"anything": "you want"}}
```
- Background refresh: `{"aps": {"content-available": 1}}`, `push_type=background`, priority 5.
- Live Activity: `push_type=liveactivity`, topic suffix `.push-type.liveactivity` (automatic),
  `{"aps": {"timestamp": 1700000000, "event": "update|end", "content-state": {...}, "alert": {...}}}`.
- VoIP: `push_type=voip` (PushKit token, topic `.voip`), payload ≤ 5 KB. Complication: `.complication`.
- Critical/sound options: `"sound": {"critical": 1, "name": "x.caf", "volume": 1.0}` (needs entitlement).
- Max payload 4 KB (5 KB VoIP). `expiration=0` → deliver once now; `collapse_id` merges updates.

## Diagnosing (the tool returns the `reason`)

BadDeviceToken (wrong env or malformed), DeviceTokenNotForTopic, Unregistered (user uninstalled —
stop sending), TopicDisallowed/MissingTopic (topic ≠ bundle id), InvalidProviderToken /
ExpiredProviderToken (key/team/time), TooManyProviderTokenUpdates, BadMessageId, PayloadTooLarge,
TooManyRequests, ServiceUnavailable (retry with backoff). 200 with an `apns-id` means APNs accepted
it; delivery itself is best-effort (background pushes are throttled by iOS).

Docs: `apple_docs_index technology=usernotifications query="sending notification requests to apns"`.
