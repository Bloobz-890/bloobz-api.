use std::{
    collections::HashMap,
    net::IpAddr,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ani_lib::{
    provider_from_show_id, AniError, AnikotoClient, AnikotoCzClient, CatalogProvider,
    TranslationType,
};
use axum::{
    body::Body,
    extract::{Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::CorsLayer;
use url::Url;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

struct AppState {
    v1: AnikotoClient,
    v2: AnikotoCzClient,
    http: reqwest::Client,
    api_key: Option<String>,
    public_url: Option<String>,
    cache: Mutex<HashMap<String, (Instant, Value)>>,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn map_err(e: AniError) -> ApiError {
    let code = match e {
        AniError::NoSearchResults | AniError::NoEpisodesAvailable { .. } => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_GATEWAY,
    };
    ApiError(code, e.to_string())
}

fn mode_of(value: &Option<String>) -> Result<TranslationType, ApiError> {
    match value {
        None => Ok(TranslationType::Sub),
        Some(v) => TranslationType::from_str(v).map_err(|e| bad(e.to_string())),
    }
}

fn provider_of(value: &Option<String>, id: Option<&str>) -> Result<CatalogProvider, ApiError> {
    match value {
        Some(v) => CatalogProvider::from_str(v).map_err(|e| bad(e.to_string())),
        None => Ok(match id {
            Some(id) => provider_from_show_id(id),
            None => CatalogProvider::Anikoto2,
        }),
    }
}

fn base_of(s: &AppState, headers: &HeaderMap) -> String {
    s.public_url.clone().unwrap_or_else(|| {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost");
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http");
        format!("{proto}://{host}")
    })
}

fn keyq(s: &AppState) -> String {
    match &s.api_key {
        Some(k) => format!("&key={}", enc(k)),
        None => String::new(),
    }
}

fn enc(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

async fn auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(expected) = &state.api_key {
        let path = req.uri().path();
        if path != "/" && path != "/health" {
            let from_header = req
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let from_query = url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
                .find(|(k, _)| k == "key")
                .map(|(_, v)| v.into_owned());
            let given = from_header.or(from_query);
            if given.as_deref() != Some(expected.as_str()) {
                return ApiError(StatusCode::UNAUTHORIZED, "invalid or missing api key".into())
                    .into_response();
            }
        }
    }
    next.run(req).await
}

async fn index() -> Json<Value> {
    Json(json!({
        "name": "ani-api",
        "description": "HTTP API over ani-cli-rs (Anikoto providers)",
        "endpoints": {
            "GET /health": "liveness check",
            "GET /search?q=&type=sub|dub&provider=anikoto2|anikoto": "search anime",
            "GET /episodes?id=&type=sub|dub": "list episodes for a show id from /search",
            "GET /stream?id=&ep=&type=sub|dub": "resolve stream links (each has a proxy_url usable by players)",
            "GET /proxy?url=&referer=&origin=": "header-injecting proxy that rewrites HLS playlists",
            "GET /home": "catalog rows: recent episodes, trending, popular, airing, top (posters, scores, genres)",
            "GET /explore?q=&genre=&year=&format=&status=&sort=&page=": "browse/search the catalog",
            "GET /info?id=": "full details for a catalog id (description, relations, recommendations)"
        }
    }))
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct SearchQ {
    q: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    provider: Option<String>,
}

async fn search(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<SearchQ>,
) -> Result<Json<Value>, ApiError> {
    let mode = mode_of(&q.kind)?;
    let provider = provider_of(&q.provider, None)?;
    let results = match provider {
        CatalogProvider::Anikoto => s.v1.search(&q.q, mode).await,
        CatalogProvider::Anikoto2 => s.v2.search(&q.q, mode).await,
    }
    .map_err(map_err)?;
    let base = base_of(&s, &headers);
    let kq = keyq(&s);
    let results: Vec<Value> = results
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert(
                    "episodes_url".into(),
                    Value::String(format!("{base}/episodes?id={}&type={mode}{kq}", enc(&r.id))),
                );
            }
            v
        })
        .collect();
    Ok(Json(json!({ "provider": provider.to_string(), "count": results.len(), "results": results })))
}

#[derive(Deserialize)]
struct EpisodesQ {
    id: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    provider: Option<String>,
}

async fn episodes(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<EpisodesQ>,
) -> Result<Json<Value>, ApiError> {
    let mode = mode_of(&q.kind)?;
    let provider = provider_of(&q.provider, Some(&q.id))?;
    let list = match provider {
        CatalogProvider::Anikoto => s.v1.episodes(&q.id, mode).await,
        CatalogProvider::Anikoto2 => s.v2.episodes(&q.id, mode).await,
    }
    .map_err(map_err)?;
    let base = base_of(&s, &headers);
    let kq = keyq(&s);
    let eps: Vec<Value> = list
        .iter()
        .map(|e| {
            json!({
                "episode": e,
                "stream_url": format!("{base}/stream?id={}&ep={}&type={mode}{kq}", enc(&q.id), enc(e))
            })
        })
        .collect();
    Ok(Json(json!({ "id": q.id, "type": mode.to_string(), "count": eps.len(), "episodes": eps })))
}

#[derive(Deserialize)]
struct StreamQ {
    id: String,
    ep: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    provider: Option<String>,
}

async fn stream(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<StreamQ>,
) -> Result<Json<Value>, ApiError> {
    let mode = mode_of(&q.kind)?;
    let provider = provider_of(&q.provider, Some(&q.id))?;
    let streams = match provider {
        CatalogProvider::Anikoto => s.v1.streams(&q.id, &q.ep, mode).await,
        CatalogProvider::Anikoto2 => s.v2.streams(&q.id, &q.ep, mode).await,
    }
    .map_err(map_err)?;

    let base = base_of(&s, &headers);

    let mut out = Vec::new();
    for link in &streams {
        let mut v = serde_json::to_value(link).unwrap_or(Value::Null);
        let mut proxy = format!("{base}/proxy?url={}", enc(&link.url));
        if let Some(r) = &link.headers.referer {
            proxy.push_str(&format!("&referer={}", enc(r)));
        }
        if let Some(o) = &link.headers.origin {
            proxy.push_str(&format!("&origin={}", enc(o)));
        }
        if let Some(k) = &s.api_key {
            proxy.push_str(&format!("&key={}", enc(k)));
        }
        if let Some(obj) = v.as_object_mut() {
            obj.insert("proxy_url".into(), Value::String(proxy));
        }
        out.push(v);
    }
    Ok(Json(json!({
        "id": q.id, "episode": q.ep, "type": mode.to_string(),
        "count": out.len(), "streams": out
    })))
}

#[derive(Deserialize)]
struct ProxyQ {
    url: String,
    referer: Option<String>,
    origin: Option<String>,
    key: Option<String>,
}

fn allowed_target(url: &Url) -> bool {
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else { return false };
    let host = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                !(v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified())
            }
            IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified()),
        };
    }
    true
}

fn proxied(target: &str, referer: &Option<String>, origin: &Option<String>, key: &Option<String>) -> String {
    let mut out = format!("/proxy?url={}", enc(target));
    if let Some(r) = referer {
        out.push_str(&format!("&referer={}", enc(r)));
    }
    if let Some(o) = origin {
        out.push_str(&format!("&origin={}", enc(o)));
    }
    if let Some(k) = key {
        out.push_str(&format!("&key={}", enc(k)));
    }
    out
}

fn rewrite_playlist(
    text: &str,
    base: &Url,
    referer: &Option<String>,
    origin: &Option<String>,
    key: &Option<String>,
) -> String {
    let mut out = String::with_capacity(text.len() + 256);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push('\n');
        } else if let Some(rest) = trimmed.strip_prefix('#') {
            if let Some(pos) = trimmed.find("URI=\"") {
                let start = pos + 5;
                if let Some(len) = trimmed[start..].find('"') {
                    let uri = &trimmed[start..start + len];
                    if let Ok(abs) = base.join(uri) {
                        out.push_str(&trimmed[..start]);
                        out.push_str(&proxied(abs.as_str(), referer, origin, key));
                        out.push_str(&trimmed[start + len..]);
                        out.push('\n');
                        continue;
                    }
                }
            }
            let _ = rest;
            out.push_str(trimmed);
            out.push('\n');
        } else if let Ok(abs) = base.join(trimmed) {
            out.push_str(&proxied(abs.as_str(), referer, origin, key));
            out.push('\n');
        } else {
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out
}

async fn proxy(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ProxyQ>,
) -> Result<Response, ApiError> {
    let target = Url::parse(&q.url).map_err(|e| bad(format!("invalid url: {e}")))?;
    if !allowed_target(&target) {
        return Err(bad("target not allowed"));
    }
    let mut req = s.http.get(target.clone()).header(header::USER_AGENT, UA);
    if let Some(r) = &q.referer {
        req = req.header(header::REFERER, r);
    }
    if let Some(o) = &q.origin {
        req = req.header(header::ORIGIN, o);
    }
    if let Some(range) = headers.get(header::RANGE) {
        req = req.header(header::RANGE, range);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("upstream error: {e}")))?;

    let status = resp.status();
    let final_url = resp.url().clone();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_playlist = final_url.path().to_ascii_lowercase().ends_with(".m3u8")
        || ctype.contains("mpegurl");

    if is_playlist && status.is_success() {
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("upstream read error: {e}")))?;
        let body = rewrite_playlist(&text, &final_url, &q.referer, &q.origin, &q.key);
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            body,
        )
            .into_response());
    }

    let mut builder = Response::builder().status(status);
    for name in [header::CONTENT_TYPE, header::CONTENT_RANGE, header::ACCEPT_RANGES, header::CONTENT_LENGTH] {
        if let Some(v) = resp.headers().get(&name) {
            if let Ok(hv) = HeaderValue::from_bytes(v.as_bytes()) {
                builder = builder.header(name, hv);
            }
        }
    }
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ---------------------------------------------------------------- catalog

const MEDIA: &str = "id idMal title{romaji english native} coverImage{extraLarge large color} bannerImage averageScore episodes format status seasonYear genres nextAiringEpisode{episode airingAt}";

async fn anilist(s: &AppState, query: String, vars: Value, ttl: u64) -> Result<Value, ApiError> {
    let key = format!("{query}|{vars}");
    let mut stale: Option<Value> = None;
    {
        let c = s.cache.lock().unwrap();
        if let Some((t, v)) = c.get(&key) {
            if t.elapsed().as_secs() < ttl {
                return Ok(v.clone());
            }
            stale = Some(v.clone());
        }
    }
    let payload = json!({ "query": query, "variables": vars }).to_string();
    let mut last = ApiError(StatusCode::BAD_GATEWAY, "catalog unavailable".into());
    for attempt in 1..=3u64 {
        let sent = s
            .http
            .post("https://graphql.anilist.co")
            .header(header::USER_AGENT, UA)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/json")
            .body(payload.clone())
            .timeout(Duration::from_secs(20))
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                last = ApiError(StatusCode::BAD_GATEWAY, format!("catalog error: {e}"));
                tokio::time::sleep(Duration::from_millis(700 * attempt)).await;
                continue;
            }
        };
        let status = resp.status();
        let wait = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(attempt)
            .min(4);
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                last = ApiError(StatusCode::BAD_GATEWAY, format!("catalog read error: {e}"));
                tokio::time::sleep(Duration::from_millis(700 * attempt)).await;
                continue;
            }
        };
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let data_ok = body.get("data").map(|d| !d.is_null()).unwrap_or(false);
        if status.is_success() && data_ok {
            let data = body["data"].clone();
            let mut c = s.cache.lock().unwrap();
            if c.len() > 400 {
                c.clear();
            }
            c.insert(key, (Instant::now(), data.clone()));
            return Ok(data);
        }
        let msg = body["errors"][0]["message"].as_str().unwrap_or("catalog unavailable").to_string();
        if status.as_u16() == 404 {
            return Err(ApiError(StatusCode::NOT_FOUND, format!("catalog: {msg} (http 404)")));
        }
        last = ApiError(StatusCode::BAD_GATEWAY, format!("catalog: {msg} (http {})", status.as_u16()));
        if status.as_u16() == 429 || status.is_server_error() {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        break;
    }
    if let Some(v) = stale {
        return Ok(v);
    }
    Err(last)
}

fn norm(m: &Value) -> Value {
    let title = m["title"]["english"].as_str().or(m["title"]["romaji"].as_str());
    json!({
        "id": m["id"],
        "mal_id": m["idMal"],
        "title": title,
        "romaji": m["title"]["romaji"],
        "native": m["title"]["native"],
        "poster": m["coverImage"]["extraLarge"],
        "poster_small": m["coverImage"]["large"],
        "color": m["coverImage"]["color"],
        "banner": m["bannerImage"],
        "score": m["averageScore"],
        "episodes": m["episodes"],
        "format": m["format"],
        "status": m["status"],
        "year": m["seasonYear"],
        "genres": m["genres"],
        "next": m["nextAiringEpisode"]
    })
}

fn norm_list(v: &Value) -> Value {
    let list: Vec<Value> = v.as_array().map(|a| a.iter().map(norm).collect()).unwrap_or_default();
    Value::Array(list)
}

fn strip_html(s: &str) -> String {
    let s = s.replace("<br>", "\n").replace("<br/>", "\n").replace("<br />", "\n");
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

async fn home(State(s): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let q = r#"query {
  trending: Page(perPage: 20) { media(type: ANIME, isAdult: false, sort: TRENDING_DESC) { MEDIA } }
  airing: Page(perPage: 20) { media(type: ANIME, isAdult: false, status: RELEASING, sort: POPULARITY_DESC) { MEDIA } }
  popular: Page(perPage: 20) { media(type: ANIME, isAdult: false, sort: POPULARITY_DESC) { MEDIA } }
  top: Page(perPage: 20) { media(type: ANIME, isAdult: false, sort: SCORE_DESC) { MEDIA } }
}"#
    .replace("MEDIA", MEDIA);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        / 300
        * 300;
    let rq = r#"query($now: Int) {
  Page(perPage: 50) {
    airingSchedules(airingAt_lesser: $now, sort: TIME_DESC) { episode airingAt media { MEDIA isAdult } }
  }
}"#
    .replace("MEDIA", MEDIA);
    let (d, rd) = tokio::join!(
        anilist(&s, q, json!({}), 1800),
        anilist(&s, rq, json!({ "now": now }), 300)
    );
    let d = d?;
    let mut recent: Vec<Value> = Vec::new();
    if let Ok(rd) = rd {
        let mut seen: Vec<i64> = Vec::new();
        for e in rd["Page"]["airingSchedules"].as_array().cloned().unwrap_or_default() {
            let m = &e["media"];
            if m.is_null() || m["isAdult"].as_bool() == Some(true) {
                continue;
            }
            let id = m["id"].as_i64().unwrap_or(0);
            if id == 0 || seen.contains(&id) {
                continue;
            }
            seen.push(id);
            let mut item = norm(m);
            if let Some(o) = item.as_object_mut() {
                o.insert("episode".into(), e["episode"].clone());
                o.insert("aired_at".into(), e["airingAt"].clone());
            }
            recent.push(item);
            if recent.len() >= 20 {
                break;
            }
        }
    }
    Ok(Json(json!({
        "recent": recent,
        "trending": norm_list(&d["trending"]["media"]),
        "airing": norm_list(&d["airing"]["media"]),
        "popular": norm_list(&d["popular"]["media"]),
        "top": norm_list(&d["top"]["media"])
    })))
}

#[derive(Deserialize)]
struct ExploreQ {
    q: Option<String>,
    genre: Option<String>,
    year: Option<i64>,
    format: Option<String>,
    status: Option<String>,
    sort: Option<String>,
    page: Option<i64>,
}

async fn explore(
    State(s): State<Arc<AppState>>,
    Query(p): Query<ExploreQ>,
) -> Result<Json<Value>, ApiError> {
    let search = p.q.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let mut vars = serde_json::Map::new();
    vars.insert("page".into(), json!(p.page.unwrap_or(1).clamp(1, 50)));
    if let Some(v) = search {
        vars.insert("search".into(), json!(v));
    }
    if let Some(v) = p.genre.as_deref().filter(|v| !v.is_empty()) {
        vars.insert("genre".into(), json!(v));
    }
    if let Some(v) = p.year {
        vars.insert("year".into(), json!(v));
    }
    if let Some(v) = p.format.as_deref().filter(|v| !v.is_empty()) {
        let f = v.to_ascii_uppercase();
        if !["TV", "TV_SHORT", "MOVIE", "OVA", "ONA", "SPECIAL"].contains(&f.as_str()) {
            return Err(bad("invalid format"));
        }
        vars.insert("format".into(), json!(f));
    }
    if let Some(v) = p.status.as_deref().filter(|v| !v.is_empty()) {
        let f = v.to_ascii_uppercase();
        if !["RELEASING", "FINISHED", "NOT_YET_RELEASED"].contains(&f.as_str()) {
            return Err(bad("invalid status"));
        }
        vars.insert("status".into(), json!(f));
    }
    let sort = match p.sort.as_deref().unwrap_or(if search.is_some() { "match" } else { "popular" }) {
        "trending" => "TRENDING_DESC",
        "score" => "SCORE_DESC",
        "latest" => "START_DATE_DESC",
        "title" => "TITLE_ROMAJI",
        "match" => "SEARCH_MATCH",
        _ => "POPULARITY_DESC",
    };
    vars.insert("sort".into(), json!([sort]));

    let q = r#"query($page: Int, $search: String, $genre: String, $year: Int, $format: MediaFormat, $status: MediaStatus, $sort: [MediaSort]) {
  Page(page: $page, perPage: 24) {
    pageInfo { hasNextPage currentPage }
    media(type: ANIME, isAdult: false, search: $search, genre: $genre, seasonYear: $year, format: $format, status: $status, sort: $sort) { MEDIA }
  }
}"#
    .replace("MEDIA", MEDIA);
    let d = anilist(&s, q, Value::Object(vars), 1800).await?;
    Ok(Json(json!({
        "page": d["Page"]["pageInfo"]["currentPage"],
        "has_next": d["Page"]["pageInfo"]["hasNextPage"],
        "results": norm_list(&d["Page"]["media"])
    })))
}

#[derive(Deserialize)]
struct InfoQ {
    id: i64,
}

async fn info(State(s): State<Arc<AppState>>, Query(p): Query<InfoQ>) -> Result<Json<Value>, ApiError> {
    let q = r#"query($id: Int) {
  Media(id: $id, type: ANIME) {
    MEDIA
    description(asHtml: false)
    duration
    popularity
    startDate { year month day }
    studios(isMain: true) { nodes { name } }
    trailer { id site }
    relations { edges { relationType node { id type format title { romaji english } coverImage { large } } } }
    recommendations(perPage: 12, sort: RATING_DESC) { nodes { mediaRecommendation { MEDIA } } }
  }
}"#
    .replace("MEDIA", MEDIA);
    let d = anilist(&s, q, json!({ "id": p.id }), 21600).await?;
    let m = &d["Media"];
    if m.is_null() {
        return Err(ApiError(StatusCode::NOT_FOUND, "not found".into()));
    }
    let mut out = norm(m);
    let relations: Vec<Value> = m["relations"]["edges"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| e["node"]["type"].as_str() == Some("ANIME"))
                .map(|e| {
                    let n = &e["node"];
                    json!({
                        "relation": e["relationType"],
                        "id": n["id"],
                        "title": n["title"]["english"].as_str().or(n["title"]["romaji"].as_str()),
                        "poster": n["coverImage"]["large"],
                        "format": n["format"]
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let recs: Vec<Value> = m["recommendations"]["nodes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|n| !n["mediaRecommendation"].is_null())
                .map(|n| norm(&n["mediaRecommendation"]))
                .collect()
        })
        .unwrap_or_default();
    let studios: Vec<Value> = m["studios"]["nodes"]
        .as_array()
        .map(|a| a.iter().map(|n| n["name"].clone()).collect())
        .unwrap_or_default();
    if let Some(o) = out.as_object_mut() {
        o.insert("description".into(), json!(strip_html(m["description"].as_str().unwrap_or(""))));
        o.insert("duration".into(), m["duration"].clone());
        o.insert("popularity".into(), m["popularity"].clone());
        o.insert("start".into(), m["startDate"].clone());
        o.insert("studios".into(), Value::Array(studios));
        o.insert("trailer".into(), m["trailer"].clone());
        o.insert("relations".into(), Value::Array(relations));
        o.insert("recommendations".into(), Value::Array(recs));
    }
    Ok(Json(out))
}

#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        v1: AnikotoClient::new().expect("anikoto client"),
        v2: AnikotoCzClient::new().expect("anikoto.cz client"),
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(8))
            .build()
            .expect("http client"),
        api_key: std::env::var("API_KEY").ok().filter(|v| !v.is_empty()),
        cache: Mutex::new(HashMap::new()),
        public_url: std::env::var("PUBLIC_URL")
            .ok()
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty()),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/search", get(search))
        .route("/episodes", get(episodes))
        .route("/stream", get(stream))
        .route("/proxy", get(proxy))
        .route("/home", get(home))
        .route("/explore", get(explore))
        .route("/info", get(info))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind");
    println!("ani-api listening on :{port}");
    axum::serve(listener, app).await.expect("server");
}
