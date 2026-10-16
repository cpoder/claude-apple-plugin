//! WeatherKit REST API.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, render};
use crate::jwt;
use crate::server::AppleServer;
use crate::util::opt;

const SECTION: &str = "weatherkit";
const BASE: &str = "https://weatherkit.apple.com/api/v1";

impl AppleServer {
    fn weather_token(&self) -> Result<String, ToolError> {
        let cfg = self.cfg();
        let key_id = cfg.require(SECTION, "key_id")?;
        let service_id = cfg.require(SECTION, "service_id")?;
        let team = cfg.team_id(SECTION)?;
        let pem = cfg.private_key(SECTION)?;
        self.tokens.get_or_mint("weatherkit", Duration::from_secs(50 * 60), || {
            let now = jwt::now();
            jwt::es256_jwt(
                &pem,
                json!({"kid": key_id, "id": format!("{team}.{service_id}")}),
                json!({"iss": team, "iat": now, "exp": now + 3600, "sub": service_id}),
            )
        })
    }

    pub(crate) async fn weather_get(&self, path: &str, query: &[(String, String)]) -> Result<Value, ToolError> {
        let token = self.weather_token()?;
        let mut req = self.http.get(format!("{BASE}/{}", path.trim_start_matches('/'))).bearer_auth(token);
        if !query.is_empty() {
            req = req.query(query);
        }
        http::json(req, &format!("WeatherKit {path}")).await
    }
}

fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut m = serde_json::Map::new();
    for k in keys {
        if let Some(x) = v.get(*k)
            && !x.is_null() {
                m.insert(k.to_string(), x.clone());
            }
    }
    Value::Object(m)
}

/// Reduce a WeatherKit response to the fields that matter for a summary.
pub fn compact_weather(v: &Value, max_hours: usize) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(cw) = v.get("currentWeather") {
        out.insert("currentWeather".into(), pick(cw, &["asOf", "conditionCode", "temperature", "temperatureApparent", "humidity", "windSpeed", "windGust", "windDirection", "precipitationIntensity", "uvIndex", "visibility", "pressure", "pressureTrend", "cloudCover", "daylight", "temperatureDewPoint"]));
    }
    if let Some(fd) = v.get("forecastDaily") {
        let days: Vec<Value> = fd["days"].as_array().cloned().unwrap_or_default().iter().map(|d| {
            let mut m = pick(d, &["forecastStart", "conditionCode", "temperatureMax", "temperatureMin", "precipitationChance", "precipitationType", "precipitationAmount", "snowfallAmount", "maxUvIndex", "sunrise", "sunset", "moonPhase", "windSpeedMax", "humidityMax"]);
            if let Some(day) = d.get("daytimeForecast") { m["daytime"] = pick(day, &["conditionCode", "precipitationChance", "windSpeed", "humidity"]); }
            if let Some(night) = d.get("overnightForecast") { m["overnight"] = pick(night, &["conditionCode", "precipitationChance", "temperatureMin"]); }
            m
        }).collect();
        out.insert("forecastDaily".into(), Value::Array(days));
    }
    if let Some(fh) = v.get("forecastHourly") {
        let hours: Vec<Value> = fh["hours"].as_array().cloned().unwrap_or_default().iter().take(max_hours).map(|h| pick(h, &["forecastStart", "conditionCode", "temperature", "temperatureApparent", "precipitationChance", "precipitationType", "precipitationAmount", "windSpeed", "windGust", "humidity", "uvIndex", "cloudCover", "daylight"])).collect();
        out.insert("forecastHourly".into(), Value::Array(hours));
    }
    if let Some(nh) = v.get("forecastNextHour") {
        let mut m = pick(nh, &["forecastStart", "forecastEnd"]);
        m["summary"] = nh["summary"].clone();
        if let Some(mins) = nh["minutes"].as_array() {
            m["minutes_preview"] = Value::Array(mins.iter().take(12).map(|x| pick(x, &["startTime", "precipitationChance", "precipitationIntensity"])).collect());
        }
        out.insert("forecastNextHour".into(), m);
    }
    if let Some(wa) = v.get("weatherAlerts") {
        let alerts: Vec<Value> = wa["alerts"].as_array().cloned().unwrap_or_default().iter().map(|a| pick(a, &["id", "name", "severity", "urgency", "certainty", "description", "effectiveTime", "expireTime", "eventOnsetTime", "eventEndTime", "source", "detailsUrl", "areaName", "countryCode"])).collect();
        out.insert("weatherAlerts".into(), Value::Array(alerts));
    }
    for key in ["currentWeather", "forecastDaily", "forecastHourly", "forecastNextHour", "weatherAlerts"] {
        if let Some(meta) = v.get(key).and_then(|x| x.get("metadata")) {
            out.insert("metadata".into(), pick(meta, &["attributionURL", "expireTime", "latitude", "longitude", "readTime", "reportedTime", "units", "sourceType"]));
            break;
        }
    }
    Value::Object(out)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WeatherParams {
    pub latitude: f64,
    pub longitude: f64,
    /// Comma-separated data sets: currentWeather, forecastDaily, forecastHourly, forecastNextHour, weatherAlerts (default currentWeather,forecastDaily)
    pub data_sets: Option<String>,
    /// Language code, e.g. en, fr (default en)
    pub language: Option<String>,
    /// IANA timezone for day boundaries, e.g. Europe/Paris
    pub timezone: Option<String>,
    /// ISO country code, required for weatherAlerts (e.g. FR)
    pub country_code: Option<String>,
    /// Start of daily forecast (ISO 8601)
    pub daily_start: Option<String>,
    /// End of daily forecast (ISO 8601)
    pub daily_end: Option<String>,
    /// Start of hourly forecast (ISO 8601)
    pub hourly_start: Option<String>,
    /// End of hourly forecast (ISO 8601)
    pub hourly_end: Option<String>,
    /// Point in time for current conditions (ISO 8601)
    pub current_as_of: Option<String>,
    /// Compact output (default true); false returns Apple's full JSON (large)
    pub compact: Option<bool>,
    /// Max hourly entries in compact mode (default 48)
    pub max_hours: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AvailabilityParams {
    pub latitude: f64,
    pub longitude: f64,
    /// ISO country code, e.g. FR
    pub country: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AlertParams {
    /// Alert id (from weatherAlerts)
    pub alert_id: String,
    /// Language code (default en)
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttributionParams {
    /// Language code (default en)
    pub language: Option<String>,
}

#[tool_router(router = weather_router, vis = "pub")]
impl AppleServer {
    /// Weather for coordinates via WeatherKit: current conditions, daily/hourly forecast, next-hour precipitation, alerts (SI units: °C, km/h, mm).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn weather(&self, Parameters(p): Parameters<WeatherParams>) -> ToolResult {
        let lang = p.language.clone().unwrap_or_else(|| "en".into());
        let mut q = vec![("dataSets".to_string(), p.data_sets.clone().unwrap_or_else(|| "currentWeather,forecastDaily".into()))];
        opt(&mut q, "timezone", &p.timezone);
        opt(&mut q, "countryCode", &p.country_code);
        opt(&mut q, "dailyStart", &p.daily_start);
        opt(&mut q, "dailyEnd", &p.daily_end);
        opt(&mut q, "hourlyStart", &p.hourly_start);
        opt(&mut q, "hourlyEnd", &p.hourly_end);
        opt(&mut q, "currentAsOf", &p.current_as_of);
        let v = self.weather_get(&format!("weather/{lang}/{}/{}", p.latitude, p.longitude), &q).await?;
        if p.compact.unwrap_or(true) {
            Ok(render(&compact_weather(&v, p.max_hours.unwrap_or(48) as usize)))
        } else {
            Ok(render(&v))
        }
    }

    /// Which WeatherKit data sets are available for a location (currentWeather, forecastDaily, forecastHourly, forecastNextHour, weatherAlerts).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn weather_availability(&self, Parameters(p): Parameters<AvailabilityParams>) -> ToolResult {
        Ok(render(&self.weather_get(&format!("availability/{}/{}", p.latitude, p.longitude), &[("country".into(), p.country.clone())]).await?))
    }

    /// Details of a weather alert by id.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn weather_alert(&self, Parameters(p): Parameters<AlertParams>) -> ToolResult {
        Ok(render(&self.weather_get(&format!("weatherAlert/{}/{}", p.language.clone().unwrap_or_else(|| "en".into()), p.alert_id), &[]).await?))
    }

    /// Attribution info (logo URLs and legal text) that must be displayed with WeatherKit data.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn weather_attribution(&self, Parameters(p): Parameters<AttributionParams>) -> ToolResult {
        Ok(render(&self.weather_get(&format!("attribution/{}", p.language.clone().unwrap_or_else(|| "en".into())), &[]).await?))
    }
}
