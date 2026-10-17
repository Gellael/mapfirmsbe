use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
mod gee;

const CO_TTL: Duration = Duration::from_secs(6 * 3600);
type CoCache = Arc<RwLock<Option<(Instant, String)>>>;
// west,south,east,north -> seluruh Indonesia
const BBOX: &str = "95,-11,141,6";
const SOURCES: [&str; 4] = [
    "VIIRS_SNPP_NRT",
    "VIIRS_NOAA20_NRT",
    "VIIRS_NOAA21_NRT",
    "MODIS_NRT",
];
const CACHE_TTL: Duration = Duration::from_secs(600);

type Cache = Arc<RwLock<HashMap<String, (Instant, Value)>>>;

#[derive(Clone)]
struct AppState {
    key: String,
    client: reqwest::Client,
    cache: Cache,
    co_cache: CoCache,
}

#[derive(Deserialize)]
struct Params {
    days: Option<u8>,
    source: Option<String>,
}

type ApiErr = (StatusCode, String);

fn upstream_err() -> ApiErr {
    // sengaja tidak membocorkan detail (URL berisi MAP_KEY)
    (StatusCode::BAD_GATEWAY, "Gagal mengambil data dari FIRMS".into())
}

async fn fires(State(s): State<AppState>, Query(p): Query<Params>) -> Result<Json<Value>, ApiErr> {
    let days = p.days.unwrap_or(1).clamp(1, 5);
    let source = p.source.unwrap_or_else(|| "VIIRS_SNPP_NRT".into());
    if !SOURCES.contains(&source.as_str()) {
        return Err((StatusCode::BAD_REQUEST, "source tidak valid".into()));
    }

    let ck = format!("{source}:{days}");
    if let Some((t, v)) = s.cache.read().await.get(&ck) {
        if t.elapsed() < CACHE_TTL {
            return Ok(Json(v.clone()));
        }
    }

    let url = format!(
        "https://firms.modaps.eosdis.nasa.gov/api/area/csv/{}/{}/{}/{}",
        s.key, source, BBOX, days
    );
    let body = s
        .client
        .get(url)
        .send()
        .await
        .map_err(|_| upstream_err())?
        .error_for_status()
        .map_err(|_| upstream_err())?
        .text()
        .await
        .map_err(|_| upstream_err())?;

    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers = rdr.headers().map_err(|_| upstream_err())?.clone();
    if !headers.iter().any(|h| h == "latitude") {
        // biasanya key salah / kuota habis
        return Err((StatusCode::BAD_GATEWAY, format!("Respons FIRMS tidak valid: {}", body.chars().take(120).collect::<String>())));
    }

    let mut features = Vec::new();
    for rec in rdr.records().flatten() {
        let mut props = Map::new();
        let (mut lat, mut lon) = (f64::NAN, f64::NAN);
        for (h, v) in headers.iter().zip(rec.iter()) {
            match h {
                "latitude" => lat = v.parse().unwrap_or(f64::NAN),
                "longitude" => lon = v.parse().unwrap_or(f64::NAN),
                "frp" => {
                    props.insert(h.into(), json!(v.parse::<f64>().unwrap_or(0.0)));
                }
                _ => {
                    props.insert(h.into(), json!(v));
                }
            }
        }
        if lat.is_nan() || lon.is_nan() {
            continue;
        }
        features.push(json!({
            "type": "Feature",
            "geometry": { "type": "Point", "coordinates": [lon, lat] },
            "properties": props
        }));
    }

    let fc = json!({ "type": "FeatureCollection", "features": features });
    s.cache.write().await.insert(ck, (Instant::now(), fc.clone()));
    Ok(Json(fc))
}

async fn co_tiles(State(s): State<AppState>) -> Result<Json<Value>, ApiErr> {
    if let Some((t, url)) = s.co_cache.read().await.as_ref() {
        if t.elapsed() < CO_TTL {
            return Ok(Json(json!({ "url": url })));
        }
    }
    let url = gee::create_co_map(&s.client, 2).await.map_err(|e| {
        eprintln!("GEE error: {e}"); // detail hanya di log, tidak dikirim ke klien
        (StatusCode::BAD_GATEWAY, "Gagal membuat layer CO".into())
    })?;
    *s.co_cache.write().await = Some((Instant::now(), url.clone()));
    Ok(Json(json!({ "url": url })))
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let key = std::env::var("FIRMS_MAP_KEY").expect("FIRMS_MAP_KEY belum diset di .env");
    let state = AppState {
        key,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap(),
        cache: Arc::new(RwLock::new(HashMap::new())),
        co_cache: Arc::new(RwLock::new(None)),
    };

    let app = Router::new()
        .route("/api/fires", get(fires))
        .route("/api/co-tiles", get(co_tiles))
        .route("/api/health", get(|| async { "ok" }))
        .layer(CorsLayer::permissive())
        .with_state(state);

    // sesudah: ikuti port dari Vercel, 8080 hanya cadangan untuk lokal
let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
let addr = format!("0.0.0.0:{port}");
let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
println!("API jalan di {addr}");
axum::serve(listener, app).await.unwrap();
}
