---
name: weatherkit
description: WeatherKit REST API reference for the apple plugin — current weather, hourly/daily forecasts, next-hour precipitation, weather alerts, availability and attribution for any coordinates. Load for weather questions or the weather / weather_* tools.
user-invocable: false
---

# WeatherKit REST API (tools `weather`, `weather_availability`, `weather_alert`, `weather_attribution`)

Auth: ES256 JWT with header `kid` + `id` (`<team_id>.<service_id>`) and claims `iss` (team),
`sub` (service id), 1-hour expiry — minted by the server from `weatherkit.*`. Setup requires a
Services ID with WeatherKit enabled **and** a key with WeatherKit enabled; a fresh setup can
return 401 for ~30 minutes until Apple activates it. The Apple Developer Program includes
500,000 calls/month; beyond that, usage is billed in tiers.

## Request

`weather latitude longitude data_sets=currentWeather,forecastDaily,forecastHourly,forecastNextHour,weatherAlerts`
- `language` (`en`, `fr`...) localizes condition names in alerts; `timezone` sets day boundaries;
  `country_code` is **required** for `weatherAlerts`.
- Windows: `daily_start/daily_end` (≤10 days), `hourly_start/hourly_end` (≤240 h, incl. past 24 h),
  `current_as_of` for historical current conditions.
- Units are SI: °C, km/h, mm, hPa, km visibility. `compact` (default) keeps the useful fields and
  caps hourly rows (`max_hours`); set `compact: false` for the raw payload.

## Reading results

- `conditionCode` values: Clear, MostlyClear, PartlyCloudy, MostlyCloudy, Cloudy, Fog, Haze, Smoky,
  Breezy, Windy, Drizzle, Rain, HeavyRain, Thunderstorms, Snow, HeavySnow, Sleet, Hail, Hot, Frigid,
  Hurricane, TropicalStorm, BlowingDust, Blizzard, Flurries, FreezingDrizzle, FreezingRain,
  IsolatedThunderstorms, ScatteredThunderstorms, SunShowers, WintryMix…
- `forecastNextHour.summary[]` describes precipitation starts/stops in the next hour;
  `minutes[]` gives per-minute chance/intensity.
- Alerts: `severity` (extreme, severe, moderate, minor), `urgency`, `certainty`,
  `detailsUrl`; fetch details with `weather_alert alert_id`.
- `weather_availability` tells which data sets exist for a location (alerts are country-dependent).

Attribution is mandatory when displaying data (Apple Weather logo + link); `weather_attribution`
returns the assets and legal text. Docs: `apple_docs_index technology=weatherkitrestapi`.
