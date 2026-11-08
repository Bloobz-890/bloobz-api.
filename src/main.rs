use std::{net::IpAddr, str::FromStr, sync::Arc, time::Duration};

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
            "GET /proxy?url=&referer=&origin=": "header-injecting proxy that rewrites HLS playlists"
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
    Query(q): Query<SearchQ>,
) -> Result<Json<Value>, ApiError> {
    let mode = mode_of(&q.kind)?;
    let provider = provider_of(&q.provider, None)?;
    let results = match provider {
        CatalogProvider::Anikoto => s.v1.search(&q.q, mode).await,
        CatalogProvider::Anikoto2 => s.v2.search(&q.q, mode).await,
    }
    .map_err(map_err)?;
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
    Query(q): Query<EpisodesQ>,
) -> Result<Json<Value>, ApiError> {
    let mode = mode_of(&q.kind)?;
    let provider = provider_of(&q.provider, Some(&q.id))?;
    let list = match provider {
        CatalogProvider::Anikoto => s.v1.episodes(&q.id, mode).await,
        CatalogProvider::Anikoto2 => s.v2.episodes(&q.id, mode).await,
    }
    .map_err(map_err)?;
    Ok(Json(json!({ "id": q.id, "type": mode.to_string(), "count": list.len(), "episodes": list })))
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

    let base = s.public_url.clone().unwrap_or_else(|| {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost");
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http");
        format!("{proto}://{host}")
    });

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
