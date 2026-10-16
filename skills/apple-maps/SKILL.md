---
name: apple-maps
description: Apple Maps Server API reference for the apple plugin — geocoding, reverse geocoding, place/POI search, autocomplete, ETAs between points, turn-by-turn directions, place details. Load for address/location/route questions or any maps_* tool.
user-invocable: false
---

# Apple Maps Server API (tools `maps_*`)

Auth: the server signs a short ES256 JWT (`iss` team id, `kid` Maps key id) and exchanges it at
`/v1/token` for a 30-minute access token (cached 25 min). Setup: a key with "Maps" enabled.
Quota: 250,000 service calls per day are included with the developer program (shared with MapKit JS).

## Tools

- `maps_geocode q="1 Infinite Loop, Cupertino"` → `results[]` with `coordinate`, `displayMapRegion`,
  `name`, `formattedAddressLines`, `structuredAddress` (locality, postCode, country...). Restrict with
  `limit_to_countries=FR`, bias with `search_location=lat,lon` or `search_region=north,east,south,west`.
- `maps_reverse_geocode latitude longitude` → nearest address(es).
- `maps_search q="coffee"` → POIs (`poiCategory`, `coordinate`, address, `alternateIds`). Filter with
  `include_poi_categories=Cafe,Restaurant` (categories: Airport, AirportGate, AirportTerminal,
  AmusementPark, ATM, Aquarium, Bakery, Bank, Beach, Brewery, Cafe, Campground, CarRental, EVCharger,
  FireStation, FitnessCenter, FoodMarket, GasStation, Hospital, Hotel, Laundry, Library, Marina,
  MovieTheater, Museum, NationalPark, Nightlife, Park, Parking, Pharmacy, Playground, Police, PostOffice,
  PublicTransport, Restaurant, Restroom, School, Stadium, Store, Theater, University, Winery, Zoo…),
  `result_type_filter=Poi|Address|PhysicalFeature|PointOfInterest`, pagination with
  `enable_pagination=true` + `page_token`.
- `maps_autocomplete q="eiff"` → completion candidates (`completionUrl` can be passed to `maps_request`).
- `maps_eta origin=48.86,2.35 destinations="48.87,2.30|48.85,2.40" transport_type=Automobile|Walking|Transit`
  → `etas[]` with `distanceMeters`, `expectedTravelTimeSeconds`, `staticTravelTimeSeconds`.
- `maps_directions origin destination transport_type=Automobile|Walking|Transit|Cycling avoid=Tolls`
  → routes with `distanceMeters`, `durationSeconds`, `hasTolls`, ordered `steps[]` (instructions);
  `requests_alternate_routes=true` for alternatives. Origins/destinations accept addresses or `lat,lon`.
- `maps_place id=…` → full place record; `maps_place id="I…,I…"` for several (alternate ids from other providers via `/v1/place/alternateIds`).

`lang=fr-FR` localizes names/instructions. Errors: 401 → key not enabled for Maps / wrong team;
429 → daily quota. Docs: `apple_docs_index technology=applemapsserverapi`.
