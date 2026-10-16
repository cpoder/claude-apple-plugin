---
name: icloud
description: iCloud access reference for the apple plugin — Calendar events (CalDAV), Contacts (CardDAV), Mail (IMAP/SMTP) with an app-specific password, and the optional Python sidecar for iCloud Drive, Photos, Find My, Reminders, Notes and Hide My Email. Load for personal iCloud data tasks (calendar, contacts, email, files, photos, find my device, reminders, notes) or icloud_* tools.
user-invocable: false
---

# iCloud (tools `icloud_*`)

Credentials in `icloud`: `apple_id` (the iCloud e-mail) + `app_specific_password` from
account.apple.com → Sign-In and Security → App-Specific Passwords (2FA required; format
`xxxx-xxxx-xxxx-xxxx`). The normal Apple ID password does not work for CalDAV/CardDAV/IMAP.

## Calendar (CalDAV, `caldav.icloud.com`)

- `icloud_calendars` → name, href, components (VEVENT / VTODO), color. Calendar hrefs are stable;
  the server discovers the principal and home once per session.
- `icloud_events start end calendar limit expand timezone` → sorted events with `href` + `etag`,
  `start/end` (UTC `Z`, or local with its TZID, or a date for all-day), `summary`, `location`,
  `description` (truncated 600 chars), `rrule`, `recurrence_id`, `organizer`, `attendees`, `alarm`.
  Recurring events are expanded to instances in the range (`expand: false` returns masters).
  Default range: next 7 days.
- `icloud_event_create calendar summary start end timezone all_day description location rrule alarm_minutes url`
  — naive times are interpreted in `timezone` (IANA, e.g. Europe/Paris) and stored in UTC;
  `start` as `YYYY-MM-DD` creates an all-day event (end exclusive, defaults to next day).
- `icloud_event_update href …` changes fields of the master event (pass `etag` from the listing to
  avoid overwriting concurrent edits); `icloud_event_delete href`.
- Reminders are **not** on CalDAV anymore (Apple moved them to CloudKit) — use the sidecar.

## Contacts (CardDAV, `contacts.icloud.com`)

- `icloud_contacts query limit` → full name, first/last, nickname, organization, job title, emails,
  phones, addresses, birthday, note, url, `href`, `etag`. Search runs server-side on FN/N/EMAIL/ORG/TEL
  (falls back to a full download + local filter if the server rejects the query).
- `icloud_contact_create full_name emails phones organization job_title note birthday url`;
  `icloud_contact_delete href`. Updating a contact: not provided (create a new one, or ask).

## Mail (IMAP `imap.mail.me.com:993`, SMTP `smtp.mail.me.com:587`)

- `icloud_mail_folders` (INBOX, Drafts, "Sent Messages", "Deleted Messages", Junk, Archive, custom).
- `icloud_mail_list folder limit unseen_only since from subject text` → uid, from, to, subject, date,
  flags (Seen, Flagged, Answered), size. UIDs are per folder.
- `icloud_mail_read uid folder max_chars mark_seen` → headers, text body (HTML stripped when there is
  no text part), attachment list (names/sizes only).
- `icloud_mail_send to cc bcc subject body html reply_to in_reply_to` — sends from `icloud.mail_from`
  (must be the @icloud.com/@me.com address or an alias; defaults to `apple_id`).
- `icloud_mail_flag uid seen flagged`; `icloud_mail_move uid destination` ("Deleted Messages" =
  trash, Archive, Junk, or any folder). Confirm with the user before moving/trashing.

## Optional sidecar: Drive, Photos, Find My, Reminders, Notes, Hide My Email

Python MCP server `apple-icloud-web` (pyicloud, private Apple web APIs — may break, needs the real
Apple ID `password` in `icloud.password` and a one-time 2FA). Register with
`/apple:setup` or `claude mcp add --scope user apple-icloud-web -- uv run --project <plugin>/sidecar/icloud-web apple-icloud-web-mcp`.
Tools: `icloud_login`, `icloud_2fa code`, `icloud_account`, `icloud_findmy_devices`,
`icloud_findmy_action device action=play_sound|display_message|lost_mode`, `icloud_drive_list|download|upload|mkdir`,
`icloud_photos_albums|list|download`, `icloud_reminders_lists`, `icloud_reminders list`,
`icloud_reminder_create`, `icloud_reminder_complete`, `icloud_notes_folders|recent`, `icloud_note`,
`icloud_hme_list|create|set_active`. If a call says 2FA is required: ask the user for the code shown
on their device and call `icloud_2fa`.

Safety: never print app-specific passwords or the Apple ID password; Find My lost mode and
device messages are visible to whoever holds the device — confirm before acting.
