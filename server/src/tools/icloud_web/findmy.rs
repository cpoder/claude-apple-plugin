//! Find My (iPhone, iPad, Mac, Watch, AirPods…) through the icloud.com `fmipservice` web API:
//! device list with battery and last known location, play sound, display message, lost mode.
//! Erase is deliberately not offered.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{WebErr, WebSession};
use crate::error::{ToolError, ToolResult};
use crate::http::render;
use crate::server::AppleServer;

const FMIP: &str = "/fmipservice/client/web";
const DEFAULT_LOST_TEXT: &str = "This device has been lost. Please call me.";
const DEFAULT_MESSAGE: &str = "Hello from Claude";

/// `clientContext` sent with every Find My call (`shouldLocate`/`selectedDevice` when locating).
fn client_context(locate: bool) -> Value {
    let mut c = json!({
        "appName": "iCloud Find (Web)",
        "appVersion": "2.0",
        "apiVersion": "3.0",
        "deviceListVersion": 1,
        "fmly": true,
        "timezone": "US/Pacific",
        "inactiveTime": 0,
    });
    if locate {
        c["shouldLocate"] = json!(true);
        c["selectedDevice"] = json!("all");
    }
    c
}

/// `refreshClient` body: the client context plus the echoed server context (`theftLoss` nulled).
pub fn refresh_body(server_context: &Value, locate: bool) -> Value {
    let mut sc = server_context.clone();
    if let Some(o) = sc.as_object_mut()
        && o.contains_key("theftLoss")
    {
        o.insert("theftLoss".into(), Value::Null);
    }
    let mut b = json!({"clientContext": client_context(locate), "serverContext": sc});
    if locate {
        b["isUpdatingAllLocations"] = json!(true);
    }
    b
}

/// Family members whose device list is still loading (sorted, for change detection).
pub fn loading_members(resp: &Value) -> Vec<String> {
    let mut v: Vec<String> = resp["userInfo"]["membersInfo"]
        .as_object()
        .map(|m| m.iter().filter(|(_, info)| info["deviceFetchStatus"] == "LOADING").map(|(k, _)| k.clone()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn ts(ms: Option<i64>) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms?).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn location_json(l: &Value) -> Value {
    let mut out = json!({
        "lat": l["latitude"],
        "lon": l["longitude"],
        "accuracy": l["horizontalAccuracy"],
        "altitude": l["altitude"],
        "timestamp": ts(l["timeStamp"].as_i64()),
        "position_type": l["positionType"],
        "is_old": l["isOld"],
        "is_inaccurate": l["isInaccurate"],
        "finished": l["locationFinished"],
    });
    // Apple sometimes attaches a reverse-geocoded address; pass it through when present.
    let addr = &l["address"];
    if let Some(lines) = addr["formattedAddressLines"].as_array() {
        out["address"] = json!(lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
    } else if let Some(s) = addr.as_str() {
        out["address"] = json!(s);
    } else if addr.is_object() {
        out["address"] = addr.clone();
    }
    out
}

/// Compact summary of a raw Find My device record.
pub fn device_json(d: &Value) -> Value {
    let features = &d["features"];
    let location = if features["LOC"].as_bool().unwrap_or(false) && d["location"].is_object() { location_json(&d["location"]) } else { Value::Null };
    let lost_capable = d["lostModeCapable"].as_bool().unwrap_or(false);
    json!({
        "id": d["id"],
        "name": d["name"],
        "model": d["deviceDisplayName"],
        "model_id": d["deviceModel"],
        "class": d["deviceClass"],
        "battery": {"level": d["batteryLevel"], "percent": d["batteryLevel"].as_f64().map(|l| (l * 100.0).round() as i64), "status": d["batteryStatus"]},
        "location": location,
        "is_locating": d["isLocating"].as_bool().unwrap_or(false),
        "lost_mode": {
            "active": lost_capable && !d["lostDevice"].is_null(),
            "capable": lost_capable,
            "enabled": d["lostModeEnabled"],
            "since": ts(d["lostTimestamp"].as_i64()),
        },
        "this_device": d["thisDevice"],
        "can": {"play_sound": features["SND"], "display_message": features["MSG"], "locate": features["LOC"]},
    })
}

/// Pick a device by id, exact name (case-insensitive) or, failing that, name substring.
pub fn match_device<'a>(devices: &'a [Value], query: &str) -> Option<&'a Value> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let lq = q.to_lowercase();
    devices
        .iter()
        .find(|d| d["id"].as_str() == Some(q) || d["name"].as_str().map(|n| n.to_lowercase() == lq).unwrap_or(false))
        .or_else(|| devices.iter().find(|d| d["name"].as_str().map(|n| n.to_lowercase().contains(&lq)).unwrap_or(false)))
}

impl WebSession {
    async fn fmip(&self, op: &str, body: &Value) -> Result<Value, WebErr> {
        let root = self.ws_url("findme")?;
        self.post(&format!("{root}{FMIP}/{op}"), &[], body).await
    }

    /// `initClient`, family polling, then (when `locate`) one `refreshClient` asking for fresh fixes.
    pub async fn findmy_devices(&self, locate: bool) -> Result<Vec<Value>, WebErr> {
        let init = json!({"clientContext": client_context(false)});
        let mut resp = match self.fmip("initClient", &init).await {
            // HTTP 450: Find My wants the session re-established; refresh it once and retry.
            Err(WebErr::AuthRequired(_)) => {
                if !self.login(false).await? {
                    return Err(WebErr::AuthRequired("two-factor code required".into()));
                }
                self.fmip("initClient", &init).await?
            }
            r => r?,
        };
        if resp["userInfo"]["hasMembers"].as_bool().unwrap_or(false) {
            let mut last: Vec<String> = Vec::new();
            for _ in 0..5 {
                let loading = loading_members(&resp);
                if loading.is_empty() || loading == last {
                    break;
                }
                last = loading;
                tokio::time::sleep(Duration::from_millis(500)).await;
                resp = self.fmip("refreshClient", &refresh_body(&resp["serverContext"], false)).await?;
            }
        }
        if locate
            && let Ok(r) = self.fmip("refreshClient", &refresh_body(&resp["serverContext"], true)).await
            && r["content"].is_array()
        {
            resp = r;
        }
        let devices = resp["content"].as_array().cloned().unwrap_or_default();
        if devices.is_empty() {
            return Err(WebErr::Other("Find My: no devices are registered to this Apple ID".into()));
        }
        Ok(devices)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindMyDevicesParams {
    /// Only devices whose name contains this text (case-insensitive)
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindMyActionParams {
    /// Device id, or its name (case-insensitive, substring accepted)
    pub device: String,
    /// `play_sound`, `display_message` or `lost_mode`
    pub action: String,
    /// Text shown on the device (display_message, lost_mode)
    pub message: Option<String>,
    /// Phone number shown on the lock screen (lost_mode)
    pub phone: Option<String>,
}

#[tool_router(router = icloud_findmy_router, vis = "pub")]
impl AppleServer {
    /// List the Find My devices of the Apple ID with battery, last known location and lost-mode state (needs icloud_login).
    #[tool(annotations(read_only_hint = true))]
    pub async fn icloud_findmy_devices(&self, Parameters(p): Parameters<FindMyDevicesParams>) -> ToolResult {
        let s = self.web().await?;
        let filter = p.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(str::to_lowercase);
        let devices: Vec<Value> = s
            .findmy_devices(true)
            .await?
            .iter()
            .filter(|d| filter.as_ref().is_none_or(|f| d["name"].as_str().map(|n| n.to_lowercase().contains(f)).unwrap_or(false)))
            .map(device_json)
            .collect();
        Ok(render(&json!({"count": devices.len(), "devices": devices})))
    }

    /// Act on a Find My device: play a sound, display a message, or enable lost mode (locks the device until unlocked with its passcode).
    #[tool(annotations(destructive_hint = true))]
    pub async fn icloud_findmy_action(&self, Parameters(p): Parameters<FindMyActionParams>) -> ToolResult {
        let s = self.web().await?;
        let devices = s.findmy_devices(false).await?;
        let d = match_device(&devices, &p.device).ok_or_else(|| {
            let names: Vec<&str> = devices.iter().filter_map(|d| d["name"].as_str()).collect();
            ToolError::new(format!("no Find My device matches `{}`; devices: {}", p.device, names.join(", ")))
        })?;
        let id = d["id"].as_str().ok_or_else(|| ToolError::new("Find My device without id"))?;
        let name = d["name"].as_str().unwrap_or("");
        let features = &d["features"];
        let action = p.action.trim().to_lowercase().replace('-', "_");
        let (op, body) = match action.as_str() {
            "play_sound" | "sound" => {
                if !features["SND"].as_bool().unwrap_or(false) {
                    return Err(ToolError::new(format!("`{name}` cannot play a sound")));
                }
                ("playSound", json!({"device": id, "subject": "Find My iPhone Alert", "clientContext": {"fmly": true}}))
            }
            "display_message" | "message" => {
                if !features["MSG"].as_bool().unwrap_or(false) {
                    return Err(ToolError::new(format!("`{name}` cannot display messages")));
                }
                let text = p.message.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or(DEFAULT_MESSAGE);
                ("sendMessage", json!({"device": id, "subject": "Find My", "userText": true, "text": text, "sound": true, "vibrate": false, "strobe": false}))
            }
            "lost_mode" | "lost" => {
                if !d["lostModeCapable"].as_bool().unwrap_or(false) {
                    return Err(ToolError::new(format!("`{name}` does not support lost mode")));
                }
                let text = p.message.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or(DEFAULT_LOST_TEXT);
                let phone = p.phone.as_deref().map(str::trim).unwrap_or("");
                ("lostDevice", json!({"text": text, "userText": true, "ownerNbr": phone, "lostModeEnabled": true, "trackingEnabled": true, "device": id, "passcode": ""}))
            }
            other => return Err(ToolError::new(format!("unknown action `{other}`: use play_sound, display_message or lost_mode"))),
        };
        let resp = s.fmip(op, &body).await?;
        let updated = resp["content"].as_array().and_then(|a| a.iter().find(|x| x["id"] == id)).map(device_json);
        Ok(render(&json!({"action": action, "device": {"id": id, "name": name}, "status": resp["statusCode"], "device_state": updated})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_matching() {
        let devs = vec![json!({"id": "AAA==", "name": "iPhone de Cyril"}), json!({"id": "BBB==", "name": "MacBook Pro"}), json!({"id": "CCC==", "name": "iphone"})];
        assert_eq!(match_device(&devs, "BBB==").unwrap()["id"], "BBB==");
        assert_eq!(match_device(&devs, "IPHONE").unwrap()["id"], "CCC==", "exact name wins over substring");
        assert_eq!(match_device(&devs, "macbook").unwrap()["id"], "BBB==");
        assert_eq!(match_device(&devs, "cyril").unwrap()["id"], "AAA==");
        assert!(match_device(&devs, "watch").is_none());
        assert!(match_device(&devs, "  ").is_none());
    }

    #[test]
    fn refresh_body_shape() {
        let b = refresh_body(&json!({"prsId": 1, "theftLoss": {"x": 1}}), true);
        assert_eq!(b["serverContext"]["theftLoss"], Value::Null);
        assert_eq!(b["serverContext"]["prsId"], 1);
        assert_eq!(b["isUpdatingAllLocations"], true);
        assert_eq!(b["clientContext"]["shouldLocate"], true);
        assert_eq!(b["clientContext"]["selectedDevice"], "all");
        let b = refresh_body(&json!({"prsId": 1}), false);
        assert!(b.get("isUpdatingAllLocations").is_none());
        assert!(b["clientContext"].get("shouldLocate").is_none());
        assert!(b["serverContext"].get("theftLoss").is_none());
        assert_eq!(b["clientContext"]["appName"], "iCloud Find (Web)");
    }

    #[test]
    fn loading_members_detected() {
        let r = json!({"userInfo": {"membersInfo": {"2": {"deviceFetchStatus": "LOADING"}, "1": {"deviceFetchStatus": "DONE"}, "3": {"deviceFetchStatus": "LOADING"}}}});
        assert_eq!(loading_members(&r), vec!["2".to_string(), "3".to_string()]);
        assert!(loading_members(&json!({})).is_empty());
    }

    #[test]
    fn device_summary() {
        let d = json!({"id": "X", "name": "iPhone", "deviceDisplayName": "iPhone 15 Pro", "batteryLevel": 0.734, "batteryStatus": "NotCharging",
            "features": {"LOC": true, "SND": true, "MSG": false}, "lostModeCapable": true, "lostDevice": null, "isLocating": false,
            "location": {"latitude": 48.8, "longitude": 2.3, "horizontalAccuracy": 10.0, "timeStamp": 1700000000000i64, "positionType": "GPS"}});
        let j = device_json(&d);
        assert_eq!(j["battery"]["percent"], 73);
        assert_eq!(j["location"]["lat"], 48.8);
        assert_eq!(j["location"]["timestamp"], "2023-11-14T22:13:20Z");
        assert_eq!(j["lost_mode"]["active"], false);
        assert_eq!(j["can"]["display_message"], false);
        let no_loc = json!({"id": "Y", "features": {"LOC": false}, "location": {"latitude": 1.0}});
        assert!(device_json(&no_loc)["location"].is_null());
    }
}
