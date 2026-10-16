"""MCP server (stdio) exposing iCloud web services that have no official API, via pyicloud.

Covered: account summary, Find My (devices, location, play sound, lost mode), iCloud Drive (list,
download, upload, mkdir), Photos (albums, list, download), Reminders (lists, items, create, complete),
Notes (folders, recent, read), Hide My Email (list, generate, reserve, deactivate).

Authentication uses the full Apple ID password (config icloud.password) plus 2FA once; the trusted
session is cached on disk. This relies on Apple's private web APIs and may break without notice.
"""

from __future__ import annotations

import json
import os
import threading
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.exceptions import ToolError

SECTION = "icloud"
MAX_OUT = 50_000

mcp = MCPServer(
    "apple-icloud-web",
    instructions=(
        "iCloud web services via pyicloud (unofficial): icloud_login/icloud_2fa first, then icloud_findmy_*, "
        "icloud_drive_*, icloud_photos_*, icloud_reminders_*, icloud_notes_*, icloud_hme_*. "
        "Credentials: icloud.apple_id + icloud.password in ~/.config/apple-mcp/config.json."
    ),
    version="0.1.0",
)

_lock = threading.RLock()
_api = None


# ---------- config ----------

def config_path() -> Path:
    p = os.environ.get("APPLE_MCP_CONFIG", "").strip()
    if p:
        return Path(p).expanduser()
    base = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    return base / "apple-mcp" / "config.json"


def load_config() -> dict[str, Any]:
    path = config_path()
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text())
    except Exception as e:  # noqa: BLE001
        raise ToolError(f"cannot parse {path}: {e}") from e


def cfg(key: str, required: bool = True) -> str | None:
    env = os.environ.get(f"APPLE_MCP_{SECTION.upper()}_{key.upper()}")
    if env:
        return env
    val = (load_config().get(SECTION) or {}).get(key)
    if val:
        return str(val)
    if required:
        raise ToolError(
            f"Missing `{SECTION}.{key}` in {config_path()} (or env APPLE_MCP_{SECTION.upper()}_{key.upper()}). "
            "The iCloud web sidecar needs apple_id and the full Apple ID password (not an app-specific password)."
        )
    return None


def session_dir() -> Path:
    d = Path(os.environ.get("APPLE_MCP_DATA_DIR") or (Path.home() / ".local" / "share" / "apple-mcp")) / "icloud-session"
    d.mkdir(parents=True, exist_ok=True)
    return d


def out(obj: Any) -> str:
    text = json.dumps(obj, indent=2, ensure_ascii=False, default=str)
    if len(text) > MAX_OUT:
        text = text[:MAX_OUT] + f"\n\n[... truncated at {MAX_OUT} chars; narrow the request ...]"
    return text


# ---------- auth ----------

def api():
    """Return an authenticated PyiCloudService or raise a helpful ToolError."""
    global _api
    with _lock:
        if _api is None:
            from pyicloud import PyiCloudService

            apple_id = cfg("apple_id")
            password = cfg("password")
            _api = PyiCloudService(apple_id, password, cookie_directory=str(session_dir()))
        if _api.requires_2fa:
            raise ToolError(
                "iCloud requires two-factor authentication: a code was sent to your trusted devices. "
                "Call icloud_2fa with that 6-digit code, then retry."
            )
        return _api


@mcp.tool(structured_output=False)
def icloud_login(force: bool = False) -> str:
    """Log in to iCloud with the Apple ID password (session cached on disk). Returns whether 2FA is needed. force=true discards the cached session."""
    global _api
    with _lock:
        if force:
            _api = None
        try:
            from pyicloud import PyiCloudService

            apple_id = cfg("apple_id")
            password = cfg("password")
            if _api is None:
                _api = PyiCloudService(apple_id, password, cookie_directory=str(session_dir()))
        except ToolError:
            raise
        except Exception as e:  # noqa: BLE001
            raise ToolError(f"iCloud login failed: {e}") from e
        return out(
            {
                "apple_id": cfg("apple_id"),
                "requires_2fa": _api.requires_2fa,
                "trusted_session": _api.is_trusted_session,
                "next_step": "call icloud_2fa with the code shown on your Apple devices" if _api.requires_2fa else "ready",
            }
        )


@mcp.tool(structured_output=False)
def icloud_2fa(code: str) -> str:
    """Submit the 6-digit two-factor code shown on your Apple devices and trust this session."""
    with _lock:
        if _api is None:
            icloud_login()
        ok = _api.validate_2fa_code(code.strip())
        if not ok:
            raise ToolError("invalid 2FA code")
        trusted = _api.trust_session()
        return out({"validated": True, "trusted_session": trusted})


# ---------- account / Find My ----------

@mcp.tool(structured_output=False)
def icloud_account() -> str:
    """Account overview: devices registered to the Apple ID, iCloud storage usage, family members."""
    a = api()
    acct = a.account
    devices = [
        {"name": getattr(d, "name", None), "model": getattr(d, "model_display_name", None) or getattr(d, "model", None), "os": getattr(d, "os_version", None), "serial": getattr(d, "serial_number", None)}
        for d in (acct.devices or [])
    ]
    storage = acct.storage
    usage = getattr(storage, "usage", None)
    st = {
        "total_bytes": getattr(usage, "total_storage_in_bytes", None),
        "used_bytes": getattr(usage, "used_storage_in_bytes", None),
        "by_app": {k: getattr(v, "usage_in_bytes", None) for k, v in (getattr(storage, "usages_by_media", {}) or {}).items()},
    }
    family = [{"name": getattr(m, "full_name", None), "apple_id": getattr(m, "apple_id", None)} for m in (acct.family or [])]
    return out({"devices": devices, "storage": st, "family": family})


def _device_summary(d) -> dict[str, Any]:
    data = d.data if hasattr(d, "data") else {}
    loc = d.location or {}
    return {
        "id": data.get("id"),
        "name": d.name,
        "model": getattr(d, "model_name", None) or data.get("deviceDisplayName"),
        "battery_level": data.get("batteryLevel"),
        "battery_status": data.get("batteryStatus"),
        "location": {
            "latitude": loc.get("latitude"),
            "longitude": loc.get("longitude"),
            "accuracy_m": loc.get("horizontalAccuracy"),
            "timestamp": datetime.fromtimestamp(loc["timeStamp"] / 1000, tz=timezone.utc).isoformat() if loc.get("timeStamp") else None,
            "type": loc.get("positionType"),
        }
        if loc
        else None,
        "lost_mode": data.get("lostModeCapable") and data.get("lostDevice") is not None,
        "capabilities": {"sound": d.sound_available, "message": d.messaging_available, "lost_mode": d.lost_mode_available, "location": d.location_available},
    }


@mcp.tool(structured_output=False)
def icloud_findmy_devices(name: str | None = None) -> str:
    """Find My: list devices with battery and last known location (optionally filter by name substring)."""
    a = api()
    devs = [_device_summary(d) for d in a.devices]
    if name:
        n = name.lower()
        devs = [d for d in devs if n in (d["name"] or "").lower()]
    return out({"devices": devs})


@mcp.tool(structured_output=False)
def icloud_findmy_action(device: str, action: str, message: str | None = None, phone: str | None = None) -> str:
    """Find My action on a device (name or id): play_sound, display_message (needs message), lost_mode (needs phone + message). Device erase is intentionally not offered."""
    a = api()
    target = None
    for d in a.devices:
        if d.name.lower() == device.lower() or (d.data.get("id") == device):
            target = d
            break
    if target is None:
        for d in a.devices:
            if device.lower() in d.name.lower():
                target = d
                break
    if target is None:
        raise ToolError(f"device `{device}` not found; see icloud_findmy_devices")
    if action == "play_sound":
        target.play_sound()
    elif action == "display_message":
        target.display_message(subject="Find My", message=message or "Hello from Claude", sounds=True)
    elif action == "lost_mode":
        if not phone:
            raise ToolError("phone is required for lost_mode")
        target.lost_device(number=phone, text=message or "This device has been lost. Please call me.")
    else:
        raise ToolError("action must be play_sound | display_message | lost_mode")
    return out({"device": target.name, "action": action, "ok": True})


# ---------- iCloud Drive ----------

def _drive_node(path: str):
    a = api()
    node = a.drive.root
    for part in [p for p in path.strip("/").split("/") if p]:
        node = node[part]
    return node


@mcp.tool(structured_output=False)
def icloud_drive_list(path: str = "/") -> str:
    """List an iCloud Drive folder (name, type, size, modified)."""
    node = _drive_node(path)
    items = []
    for child in node.get_children():
        items.append({"name": child.name, "type": child.type, "size": child.size, "modified": child.date_modified.isoformat() if child.date_modified else None})
    return out({"path": path, "items": items})


@mcp.tool(structured_output=False)
def icloud_drive_download(path: str, destination: str) -> str:
    """Download an iCloud Drive file to a local path."""
    node = _drive_node(path)
    dest = Path(destination).expanduser()
    if dest.is_dir():
        dest = dest / node.name
    dest.parent.mkdir(parents=True, exist_ok=True)
    with node.open(stream=True) as resp, open(dest, "wb") as fh:
        for chunk in resp.iter_content(1 << 16):
            fh.write(chunk)
    return out({"downloaded": str(dest), "bytes": dest.stat().st_size})


@mcp.tool(structured_output=False)
def icloud_drive_upload(local_path: str, folder: str = "/") -> str:
    """Upload a local file into an iCloud Drive folder."""
    src = Path(local_path).expanduser()
    if not src.is_file():
        raise ToolError(f"{src} is not a file")
    node = _drive_node(folder)
    with open(src, "rb") as fh:
        node.upload(fh)
    return out({"uploaded": src.name, "folder": folder})


@mcp.tool(structured_output=False)
def icloud_drive_mkdir(parent: str, name: str) -> str:
    """Create a folder in iCloud Drive."""
    node = _drive_node(parent)
    node.mkdir(name)
    return out({"created": f"{parent.rstrip('/')}/{name}"})


# ---------- Photos ----------

@mcp.tool(structured_output=False)
def icloud_photos_albums() -> str:
    """List iCloud Photos albums with photo counts."""
    a = api()
    albums = []
    for name, album in a.photos.albums.items():
        try:
            n = len(album)
        except Exception:  # noqa: BLE001
            n = None
        albums.append({"name": name, "count": n})
    return out({"albums": albums})


@mcp.tool(structured_output=False)
def icloud_photos_list(album: str = "All Photos", limit: int = 30, offset: int = 0) -> str:
    """List photos/videos of an album (filename, date, size, dimensions), newest first."""
    a = api()
    try:
        alb = a.photos.albums[album]
    except KeyError as e:
        raise ToolError(f"album `{album}` not found; see icloud_photos_albums") from e
    items = []
    for i, p in enumerate(alb.photos):
        if i < offset:
            continue
        if len(items) >= limit:
            break
        items.append({"id": p.id, "filename": p.filename, "date": p.asset_date.isoformat() if p.asset_date else None, "size": p.size, "dimensions": p.dimensions, "type": p.item_type, "favorite": getattr(p, "is_favorite", None)})
    return out({"album": album, "items": items})


@mcp.tool(structured_output=False)
def icloud_photos_download(filename: str, destination: str, album: str = "All Photos", version: str = "original") -> str:
    """Download a photo/video by filename from an album to a local path (version: original | medium | thumb)."""
    a = api()
    alb = a.photos.albums[album]
    for p in alb.photos:
        if p.filename == filename:
            data = p.download(version)
            if data is None:
                raise ToolError("download returned no data")
            dest = Path(destination).expanduser()
            if dest.is_dir():
                dest = dest / filename
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(data)
            return out({"downloaded": str(dest), "bytes": len(data)})
    raise ToolError(f"{filename} not found in album {album}")


# ---------- Reminders ----------

@mcp.tool(structured_output=False)
def icloud_reminders_lists() -> str:
    """List Reminders lists (id, name)."""
    a = api()
    lists = [{"id": getattr(lst, "id", None), "name": getattr(lst, "name", None) or getattr(lst, "title", None)} for lst in a.reminders.lists()]
    return out({"lists": lists})


def _list_id(a, list_ref: str) -> str:
    for lst in a.reminders.lists():
        lid = getattr(lst, "id", None)
        name = getattr(lst, "name", None) or getattr(lst, "title", None) or ""
        if list_ref == lid or list_ref.lower() == name.lower():
            return lid
    raise ToolError(f"reminders list `{list_ref}` not found; see icloud_reminders_lists")


def _reminder_json(r) -> dict[str, Any]:
    return {
        "id": getattr(r, "id", None),
        "title": getattr(r, "title", None),
        "notes": getattr(r, "description", None) or getattr(r, "notes", None),
        "completed": getattr(r, "completed", None),
        "due": getattr(r, "due_date", None),
        "priority": getattr(r, "priority", None),
        "flagged": getattr(r, "flagged", None),
    }


@mcp.tool(structured_output=False)
def icloud_reminders(list: str, include_completed: bool = False, limit: int = 100) -> str:  # noqa: A002
    """List reminders of a list (by id or name)."""
    a = api()
    lid = _list_id(a, list)
    res = a.reminders.list_reminders(lid, include_completed=include_completed, results_limit=limit)
    items = [_reminder_json(r) for r in getattr(res, "reminders", res)]
    return out({"list": list, "reminders": items})


@mcp.tool(structured_output=False)
def icloud_reminder_create(list: str, title: str, notes: str = "", due: str | None = None, priority: int = 0, all_day: bool = False) -> str:  # noqa: A002
    """Create a reminder in a list (due: ISO 8601 date/time)."""
    a = api()
    lid = _list_id(a, list)
    due_dt = datetime.fromisoformat(due) if due else None
    r = a.reminders.create(lid, title, desc=notes, due_date=due_dt, priority=priority, all_day=all_day)
    return out({"created": _reminder_json(r)})


@mcp.tool(structured_output=False)
def icloud_reminder_complete(reminder_id: str, completed: bool = True) -> str:
    """Mark a reminder completed (or not) by id."""
    a = api()
    r = a.reminders.get(reminder_id)
    r.completed = completed
    a.reminders.update(r)
    return out({"updated": _reminder_json(r)})


# ---------- Notes ----------

@mcp.tool(structured_output=False)
def icloud_notes_folders() -> str:
    """List Notes folders (id, name)."""
    a = api()
    folders = [{"id": getattr(f, "id", None), "name": getattr(f, "name", None) or getattr(f, "title", None)} for f in a.notes.folders()]
    return out({"folders": folders})


@mcp.tool(structured_output=False)
def icloud_notes_recent(limit: int = 20, folder_id: str | None = None) -> str:
    """List recent notes (id, title, modified), optionally within a folder."""
    a = api()
    it = a.notes.recents_in_folder(folder_id, limit=limit) if folder_id else a.notes.recents(limit=limit)
    notes = [{"id": getattr(n, "id", None), "title": getattr(n, "title", None), "modified": getattr(n, "modified", None) or getattr(n, "modification_date", None), "snippet": getattr(n, "snippet", None)} for n in it]
    return out({"notes": notes})


@mcp.tool(structured_output=False)
def icloud_note(note_id: str, max_chars: int = 20000) -> str:
    """Read a note's content (rendered as text/markdown) by id."""
    a = api()
    text = a.notes.render_note(note_id)
    if len(text) > max_chars:
        text = text[:max_chars] + "\n[... truncated ...]"
    return text


# ---------- Hide My Email ----------

@mcp.tool(structured_output=False)
def icloud_hme_list() -> str:
    """List Hide My Email aliases (address, label, active)."""
    a = api()
    items = [{"anonymous_id": e.get("anonymousId"), "email": e.get("hme"), "label": e.get("label"), "note": e.get("note"), "active": e.get("isActive"), "forward_to": e.get("forwardToEmail")} for e in a.hidemyemail]
    return out({"aliases": items})


@mcp.tool(structured_output=False)
def icloud_hme_create(label: str, note: str = "Generated by Claude") -> str:
    """Generate and reserve a new Hide My Email alias with a label."""
    a = api()
    email = a.hidemyemail.generate()
    if not email:
        raise ToolError("Apple did not return an address (rate limited?)")
    res = a.hidemyemail.reserve(email, label, note)
    return out({"email": email, "label": label, "result": res})


@mcp.tool(structured_output=False)
def icloud_hme_set_active(anonymous_id: str, active: bool) -> str:
    """Deactivate (stop forwarding) or reactivate a Hide My Email alias."""
    a = api()
    res = a.hidemyemail.reactivate(anonymous_id) if active else a.hidemyemail.deactivate(anonymous_id)
    return out({"anonymous_id": anonymous_id, "active": active, "result": res})


def main() -> None:
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
