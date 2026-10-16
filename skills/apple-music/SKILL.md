---
name: apple-music
description: Apple Music API reference for the apple plugin — catalog search, songs/albums/artists/playlists lookup, charts, storefronts, and (with a Music User Token) the user's library, recently played, recommendations, playlist creation. Load for any music_* tool or Apple Music / MusicKit question.
user-invocable: false
---

# Apple Music API (tools `music_*`)

Developer token: ES256 JWT (`iss` = team id, `kid` = MusicKit key id) valid up to 6 months —
the server mints one for 12 h from `music.key_id/team_id/private_key`. Catalog calls need
only this token. Anything under `/v1/me/...` additionally needs a **Music User Token**
(`music.user_token`), obtained in a client with MusicKit (`MusicKit.getInstance().authorize()`
on the web, `MusicAuthorization.request()` + `DefaultMusicTokenProvider` on Apple platforms);
user tokens expire after ~6 months.

Storefront: two-letter code (`fr`, `us`, `gb`, `jp`...), default `music.storefront`;
`music_library kind=storefront` returns the user's own. `l=fr-FR` localizes names.

## Catalog

- `music_search term="…" types=songs,albums,artists,playlists` (limit ≤25 per type).
- `music_get resource_type=albums id=<id> relationship=tracks`; artists: `views=top-songs,latest-release`;
  multi-get with comma-separated ids. Song ISRC lookup: `music_request GET /v1/catalog/fr/songs?filter[isrc]=…`.
- `music_charts types=songs,albums genre=21` (genre ids via `music_request GET /v1/catalog/fr/genres`),
  city charts `chart=city-chart-paris`... (check `/v1/catalog/{sf}/charts?types=…&with=cityCharts`).
- Search hints/suggestions: `GET /v1/catalog/{sf}/search/suggestions?term=…&kinds=terms,topResults`.
- Stations: `types=stations`; curators and editorial playlists via `curators`, `apple-curators`.
- Equivalents in another storefront: `GET /v1/catalog/{sf}/songs?filter[equivalents]=<id>`.

## Library (user token)

`music_library kind=songs|albums|artists|playlists|recently-added|recently-played|recommendations|heavy-rotation`;
`music_library_add resource_type=songs ids=…`; `music_playlist_create name=… track_ids=…`;
`music_playlist_add_tracks playlist_id=p.xxx track_ids=…`. Rating: `PUT /v1/me/ratings/songs/{id}`
`{"attributes":{"value":1}}` (1 love, -1 dislike). Library ids start with `i.` (songs) / `p.` (playlists)
and only work with user-token endpoints.

## Output

`compact` (default) replaces artwork objects by a 300×300 URL and drops playParams/previews.
Pass `compact: false` to `music_request` for everything. Responses paginate with `next`
(relative URL — pass it as `path` to `music_request`).

Errors: 401 → token/team/key mismatch or MusicKit not enabled on the key; 403 on `/me` → user token missing/expired.
Docs: `apple_docs_index technology=applemusicapi`.
