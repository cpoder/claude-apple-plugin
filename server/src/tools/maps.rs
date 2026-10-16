//! Apple Maps Server API: geocoding, search, autocomplete, ETAs, directions, places.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{self, query_pairs, render};
use crate::jwt;
use crate::server::AppleServer;
use crate::util::{opt, opt_bool};

const SECTION: &str = "maps";
const BASE: &str = "https://maps-api.apple.com/v1";

impl AppleServer {
    pub(crate) async fn maps_access_token(&self) -> Result<String, ToolError> {
        if let Some(t) = self.tokens.get("maps") {
            return Ok(t);
        }
        let cfg = self.cfg();
        let key_id = cfg.require(SECTION, "key_id")?;
        let team = cfg.team_id(SECTION)?;
        let pem = cfg.private_key(SECTION)?;
        let now = jwt::now();
        let auth_jwt = jwt::es256_jwt(&pem, json!({"kid": key_id, "typ": "JWT"}), json!({"iss": team, "iat": now, "exp": now + 1800}))?;
        let v = http::json(self.http.get(format!("{BASE}/token")).bearer_auth(auth_jwt), "Maps token exchange").await?;
        let token = v["accessToken"].as_str().ok_or_else(|| ToolError::new(format!("no accessToken in Maps token response: {v}")))?.to_string();
        let ttl = v["expiresInSeconds"].as_u64().unwrap_or(1800).saturating_sub(120).max(60);
        self.tokens.put("maps", token.clone(), Duration::from_secs(ttl));
        Ok(token)
    }

    async fn maps_get(&self, path: &str, query: &[(String, String)]) -> Result<Value, ToolError> {
        let token = self.maps_access_token().await?;
        let url = if path.starts_with("http") { path.to_string() } else { format!("{BASE}/{}", path.trim_start_matches('/')) };
        let mut req = self.http.get(&url).bearer_auth(token);
        if !query.is_empty() {
            req = req.query(query);
        }
        http::json(req, &format!("Maps {path}")).await
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MapsRequestParams {
    /// Path under /v1, e.g. `geocode`, `search`, `etas`, `directions`, `place/{id}`
    pub path: String,
    /// Query params
    pub query: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GeocodeParams {
    /// Address or place text to geocode
    pub q: String,
    /// Comma-separated ISO country codes to restrict results, e.g. FR,BE
    pub limit_to_countries: Option<String>,
    /// Language, e.g. fr-FR
    pub lang: Option<String>,
    /// Bias location "lat,lon"
    pub search_location: Option<String>,
    /// Bias region "northLat,eastLon,southLat,westLon"
    pub search_region: Option<String>,
    /// User location "lat,lon"
    pub user_location: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReverseGeocodeParams {
    pub latitude: f64,
    pub longitude: f64,
    /// Language, e.g. fr-FR
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Search text (place name, category, address)
    pub q: String,
    /// Bias location "lat,lon"
    pub search_location: Option<String>,
    /// Region "northLat,eastLon,southLat,westLon"
    pub search_region: Option<String>,
    /// User location "lat,lon"
    pub user_location: Option<String>,
    /// Language, e.g. fr-FR
    pub lang: Option<String>,
    /// Comma-separated result types: Poi, Address, PhysicalFeature, PointOfInterest, Query
    pub result_type_filter: Option<String>,
    /// Comma-separated POI categories to include, e.g. Restaurant,Cafe,Hotel,Airport,Museum
    pub include_poi_categories: Option<String>,
    /// POI categories to exclude
    pub exclude_poi_categories: Option<String>,
    /// Restrict to ISO country codes, e.g. FR
    pub limit_to_countries: Option<String>,
    /// Enable pagination (default false)
    pub enable_pagination: Option<bool>,
    /// Page token from a previous paginated response
    pub page_token: Option<String>,
    /// Restrict the search to the region (default false)
    pub search_region_priority: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EtaParams {
    /// Origin "lat,lon"
    pub origin: String,
    /// Destinations "lat,lon|lat,lon" (up to 10)
    pub destinations: String,
    /// Automobile (default), Walking, Transit
    pub transport_type: Option<String>,
    /// ISO 8601 departure date (default now)
    pub departure_date: Option<String>,
    /// ISO 8601 arrival date
    pub arrival_date: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DirectionsParams {
    /// Origin: address text or "lat,lon"
    pub origin: String,
    /// Destination: address text or "lat,lon"
    pub destination: String,
    /// Automobile (default), Walking, Transit, Cycling
    pub transport_type: Option<String>,
    /// Comma-separated features to avoid: Tolls
    pub avoid: Option<String>,
    /// ISO 8601 departure date
    pub departure_date: Option<String>,
    /// ISO 8601 arrival date
    pub arrival_date: Option<String>,
    /// Language, e.g. fr-FR
    pub lang: Option<String>,
    /// Return alternate routes (default false)
    pub requests_alternate_routes: Option<bool>,
    /// Bias "lat,lon" for resolving textual origin/destination
    pub user_location: Option<String>,
    /// Max steps to list per route in the output (default 40)
    pub max_steps: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlaceParams {
    /// Place id(s) from search results (comma-separated for several)
    pub id: String,
    /// Language, e.g. fr-FR
    pub lang: Option<String>,
}

fn common(q: &mut Vec<(String, String)>, lang: &Option<String>, search_location: &Option<String>, search_region: &Option<String>, user_location: &Option<String>, limit_to_countries: &Option<String>) {
    opt(q, "lang", lang);
    opt(q, "searchLocation", search_location);
    opt(q, "searchRegion", search_region);
    opt(q, "userLocation", user_location);
    opt(q, "limitToCountries", limit_to_countries);
}

#[tool_router(router = maps_router, vis = "pub")]
impl AppleServer {
    /// Generic Apple Maps Server API GET request.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn maps_request(&self, Parameters(p): Parameters<MapsRequestParams>) -> ToolResult {
        Ok(render(&self.maps_get(&p.path, &query_pairs(&p.query)).await?))
    }

    /// Geocode an address/place name to coordinates with structured address (Apple Maps).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn maps_geocode(&self, Parameters(p): Parameters<GeocodeParams>) -> ToolResult {
        let mut q = vec![("q".to_string(), p.q.clone())];
        common(&mut q, &p.lang, &p.search_location, &p.search_region, &p.user_location, &p.limit_to_countries);
        Ok(render(&self.maps_get("geocode", &q).await?))
    }

    /// Reverse geocode coordinates to an address (Apple Maps).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn maps_reverse_geocode(&self, Parameters(p): Parameters<ReverseGeocodeParams>) -> ToolResult {
        let mut q = vec![("loc".to_string(), format!("{},{}", p.latitude, p.longitude))];
        opt(&mut q, "lang", &p.lang);
        Ok(render(&self.maps_get("reverseGeocode", &q).await?))
    }

    /// Search places and points of interest (restaurants, hotels, shops...) near a location or in a region.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn maps_search(&self, Parameters(p): Parameters<SearchParams>) -> ToolResult {
        let mut q = vec![("q".to_string(), p.q.clone())];
        common(&mut q, &p.lang, &p.search_location, &p.search_region, &p.user_location, &p.limit_to_countries);
        opt(&mut q, "resultTypeFilter", &p.result_type_filter);
        opt(&mut q, "includePoiCategories", &p.include_poi_categories);
        opt(&mut q, "excludePoiCategories", &p.exclude_poi_categories);
        opt(&mut q, "searchRegionPriority", &p.search_region_priority);
        opt_bool(&mut q, "enablePagination", &p.enable_pagination);
        opt(&mut q, "pageToken", &p.page_token);
        Ok(render(&self.maps_get("search", &q).await?))
    }

    /// Autocomplete suggestions for a partial place/address query.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn maps_autocomplete(&self, Parameters(p): Parameters<SearchParams>) -> ToolResult {
        let mut q = vec![("q".to_string(), p.q.clone())];
        common(&mut q, &p.lang, &p.search_location, &p.search_region, &p.user_location, &p.limit_to_countries);
        opt(&mut q, "resultTypeFilter", &p.result_type_filter);
        opt(&mut q, "includePoiCategories", &p.include_poi_categories);
        opt(&mut q, "excludePoiCategories", &p.exclude_poi_categories);
        Ok(render(&self.maps_get("searchAutocomplete", &q).await?))
    }

    /// Estimated travel times/distances from one origin to up to 10 destinations.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn maps_eta(&self, Parameters(p): Parameters<EtaParams>) -> ToolResult {
        let mut q = vec![("origin".to_string(), p.origin.clone()), ("destinations".to_string(), p.destinations.clone())];
        opt(&mut q, "transportType", &p.transport_type);
        opt(&mut q, "departureDate", &p.departure_date);
        opt(&mut q, "arrivalDate", &p.arrival_date);
        Ok(render(&self.maps_get("etas", &q).await?))
    }

    /// Turn-by-turn directions between two places (driving, walking, transit, cycling) with distance, duration and steps.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn maps_directions(&self, Parameters(p): Parameters<DirectionsParams>) -> ToolResult {
        let mut q = vec![("origin".to_string(), p.origin.clone()), ("destination".to_string(), p.destination.clone())];
        opt(&mut q, "transportType", &p.transport_type);
        opt(&mut q, "avoid", &p.avoid);
        opt(&mut q, "departureDate", &p.departure_date);
        opt(&mut q, "arrivalDate", &p.arrival_date);
        opt(&mut q, "lang", &p.lang);
        opt(&mut q, "userLocation", &p.user_location);
        opt_bool(&mut q, "requestsAlternateRoutes", &p.requests_alternate_routes);
        let mut v = self.maps_get("directions", &q).await?;
        let max_steps = p.max_steps.unwrap_or(40) as usize;
        // steps are referenced by index from routes[].stepIndexes; inline a readable step list per route
        let steps = v["steps"].as_array().cloned().unwrap_or_default();
        if let Some(routes) = v["routes"].as_array_mut() {
            for r in routes.iter_mut() {
                let idx: Vec<usize> = r["stepIndexes"].as_array().map(|a| a.iter().filter_map(|i| i.as_u64()).map(|i| i as usize).collect()).unwrap_or_default();
                let listed: Vec<Value> = idx.iter().take(max_steps).filter_map(|i| steps.get(*i)).map(|s| json!({"instructions": s["instructions"], "distanceMeters": s["distanceMeters"], "durationSeconds": s["durationSeconds"], "transportType": s["transportType"]})).collect();
                r["steps"] = Value::Array(listed);
                if idx.len() > max_steps {
                    r["steps_omitted"] = json!(idx.len() - max_steps);
                }
                r.as_object_mut().map(|o| o.remove("stepIndexes"));
            }
        }
        v.as_object_mut().map(|o| o.remove("steps"));
        v.as_object_mut().map(|o| o.remove("stepPaths"));
        Ok(render(&v))
    }

    /// Place details by id (name, address, coordinates, category, phone/website when available).
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn maps_place(&self, Parameters(p): Parameters<PlaceParams>) -> ToolResult {
        let mut q: Vec<(String, String)> = Vec::new();
        opt(&mut q, "lang", &p.lang);
        if p.id.contains(',') {
            q.push(("ids".into(), p.id.clone()));
            Ok(render(&self.maps_get("place", &q).await?))
        } else {
            Ok(render(&self.maps_get(&format!("place/{}", p.id), &q).await?))
        }
    }
}
