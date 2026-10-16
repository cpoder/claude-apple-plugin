---
name: icloud
description: iCloud access reference for the apple plugin — Calendar events (CalDAV), Contacts (CardDAV), Mail (IMAP/SMTP) with an app-specific password, and the iCloud web tools (Apple ID password + 2FA) for Reminders, Notes, iCloud Drive, Photos, Find My and Hide My Email. Load for personal iCloud data tasks (calendar, contacts, email, files, photos, find my device, reminders, notes) or icloud_* tools.
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
- Reminders are **not** on CalDAV anymore (Apple moved them to CloudKit) — use the iCloud web tools below.

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

## iCloud web: Reminders, Notes, Drive, Photos, Find My, Hide My Email

Built into the same Rust server, through the private icloud.com web APIs (what the web app and
pyicloud use — may break without notice). Needs the real Apple ID `password` in `icloud.password`
(not an app-specific password) and one two-factor code at first login; the trusted session is then
cached on disk (plugin data dir, `icloud-session/`).

Login: any tool logs in silently from the cache. If it answers that 2FA is required, Apple has
pushed a 6-digit code to the user's trusted devices: ask for it and call `icloud_2fa code`. No
prompt on the devices → `icloud_login sms=true` sends it by SMS to the trusted phone. If Apple
asks to accept updated iCloud terms, the user must sign in once at https://www.icloud.com/.
`icloud_login force=true` discards the cached session. From a shell (MCP not connected):
`bash scripts/apple-mcp icloud login|sms|2fa <code>|lists|reminders <list>`.

- Reminders (CloudKit zone `Reminders`): `icloud_reminders_lists`; `icloud_reminders list include_completed limit`
  (list by id or name); `icloud_reminder_create list title notes due time_zone priority all_day flagged parent_reminder_id`
  (priority 0 none / 1 high / 5 medium / 9 low); `icloud_reminder_complete reminder_id completed`;
  `icloud_reminder_delete reminder_id` (soft delete, confirm first).
- Notes: `icloud_notes_folders`; `icloud_notes_recent limit folder_id`; `icloud_note note_id max_chars`
  (body rendered as Markdown: headings, checklists, tables; locked notes are not readable).
- Drive: `icloud_drive_list path`; `icloud_drive_download path destination overwrite`;
  `icloud_drive_upload local_path folder`; `icloud_drive_mkdir parent name`.
- Photos: `icloud_photos_albums`; `icloud_photos_list album limit offset` (default album "Library", newest first);
  `icloud_photos_download filename destination album version=original|medium|thumb`.
- Find My: `icloud_findmy_devices name`; `icloud_findmy_action device action=play_sound|display_message|lost_mode message phone`
  (no erase).
- Hide My Email: `icloud_hme_list`; `icloud_hme_create label note`; `icloud_hme_set_active anonymous_id active`;
  `icloud_hme_delete anonymous_id`.
- `icloud_account`: devices, storage usage, family.

Safety: never print app-specific passwords or the Apple ID password; Find My lost mode and
device messages are visible to whoever holds the device — confirm before acting.
