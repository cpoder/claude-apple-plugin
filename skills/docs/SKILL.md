---
name: docs
description: Look up Apple developer documentation (frameworks, APIs, REST endpoints, Swift/Objective-C symbols, articles) and render it as Markdown. Use for questions about SwiftUI, UIKit, Foundation, App Store Connect API endpoints, WeatherKit, MusicKit, CloudKit, Xcode, visionOS... or when the user runs /apple:docs <query>.
argument-hint: "<technology> <query>"
---

# /apple:docs — Apple developer documentation

Query: `$ARGUMENTS`

Strategy (all public, no credentials):

1. If the query already looks like a documentation path or URL (`swiftui/view`,
   `https://developer.apple.com/documentation/...`), call `apple_docs` directly.
2. Otherwise identify the technology: if unsure of the slug, call `apple_technologies` with a
   keyword (e.g. `weather`, `music`, `notification`). Common slugs: `swiftui`, `uikit`,
   `foundation`, `swift`, `combine`, `coredata`, `swiftdata`, `cloudkit`, `storekit`,
   `appstoreconnectapi`, `appstoreserverapi`, `applemusicapi`, `weatherkitrestapi`,
   `applemapsserverapi`, `usernotifications`, `xcode`, `visionos`, `realitykit`, `arkit`.
3. Call `apple_docs_index` with the technology and the remaining words as `query` to get page
   paths, then `apple_docs` on the best match (`section: overview` for API shape only,
   `topics` to browse members).
4. Answer with the declaration, availability, key discussion points and a link
   (`https://developer.apple.com/documentation/<path>`). Quote code from the page when useful.

Tips: paths are lowercase; symbols nest under their type (`swiftui/view/padding(_:)`);
REST endpoints appear as `GET /v1/...` titles in `appstoreconnectapi`.
