---
name: apple-news
description: Apple News Publisher API reference for the apple plugin — channel info, sections, listing/reading articles, publishing or updating Apple News Format articles with HMAC authentication. Load for Apple News publishing tasks or news_* tools.
user-invocable: false
---

# Apple News Publisher API (tools `news_*`)

Auth: `Authorization: HHMAC; key=<key_id>; signature=<base64 HMAC-SHA256>; date=<ISO 8601>` where
the canonical string is `METHOD + URL + date` (+ `Content-Type` + body for POST). The server signs
each request from `news.channel_id/key_id/key_secret` (News Publisher → channel → API keys).
Base `https://news-api.apple.com`.

## Tools

- `news_channel` → name, website, `shareUrl`, `createdAt`, quotas.
- `news_sections` → section ids/names (needed to place an article; the default section is `isDefault`).
- `news_articles limit sort page_token from_date` → `data[]`: `id`, `title`, `state`
  (PROCESSING, LIVE, PROCESSING_UPDATE, TAKEN_DOWN, FAILED_PROCESSING, DUPLICATE),
  `isPreview`, `isSponsored`, `maturityRating`, `warnings`, `shareUrl`, `revision`, `createdAt/modifiedAt`.
- `news_article article_id` → full metadata incl. `revision` and processing `warnings`.
- `news_publish_article article={ANF JSON} metadata={"data": {"isPreview": true, "links": {"sections": ["https://news-api.apple.com/sections/<id>"]}}}`
  creates; pass `article_id` + `metadata.data.revision` (current revision from `news_article`) to update.
  Images referenced as `bundle://name` must be added as extra multipart parts — not supported here;
  use absolute `https://` URLs for images instead.
- Delete: `news_request DELETE /articles/{id}`. Search: `GET /channels/{id}/articles?...` or
  `GET /sections/{id}/articles`.

## Apple News Format minimum

```json
{"version": "1.7", "identifier": "my-article-1", "title": "Title", "language": "fr",
 "layout": {"columns": 7, "width": 1024}, "components": [{"role": "title", "text": "Title"}, {"role": "body", "text": "Paragraph…", "format": "markdown"}],
 "componentTextStyles": {"default": {"fontName": "Helvetica", "fontSize": 18}}, "metadata": {"excerpt": "…", "authors": ["Name"]}}
```
Docs: `apple_docs_index technology=applenewsapi`, `apple_news_format`.
