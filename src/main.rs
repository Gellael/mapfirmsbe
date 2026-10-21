use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
mod gee;

const TILE_TTL: Duration = Duration::from_secs(6 * 3600);
const ZONAL_TTL: Duration = Duration::from_secs(3600);
const CO_DAYS: i64 = 2;
const BURN_DAYS: i64 = 120; // MCD64A1 bulanan & tertunda 1-2 bulan, jadi jendelanya panjang
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
    tile_cache: Cache,  // kunci: "co" / "burned"
    zonal_cache: Cache, // kunci: jenis + hash geometri
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

/// Lapisan raster GEE (di-cache 6 jam per jenis).
async fn tiles(s: &AppState, kind: &'static str, days: i64) -> Result<Json<Value>, ApiErr> {
    if let Some((t, v)) = s.tile_cache.read().await.get(kind) {
        if t.elapsed() < TILE_TTL {
            return Ok(Json(v.clone()));
        }
    }
    let (url, start, end) = gee::create_map(&s.client, kind, days).await.map_err(|e| {
        eprintln!("GEE error ({kind}): {e}"); // detail hanya di log, tidak dikirim ke klien
        (StatusCode::BAD_GATEWAY, format!("Gagal membuat layer {kind}"))
    })?;
    let v = json!({ "url": url, "start": start, "end": end });
    s.tile_cache.write().await.insert(kind.to_string(), (Instant::now(), v.clone()));
    Ok(Json(v))
}

async fn co_tiles(State(s): State<AppState>) -> Result<Json<Value>, ApiErr> {
    tiles(&s, "co", CO_DAYS).await
}

async fn burned_tiles(State(s): State<AppState>) -> Result<Json<Value>, ApiErr> {
    tiles(&s, "burned", BURN_DAYS).await
}

#[derive(Deserialize)]
struct ZonalBody {
    coords: Value, // koordinat MultiPolygon GeoJSON: [poligon][cincin][titik][lon,lat]
}

/// Validasi bentuk & ukuran supaya klien tidak bisa mengirim ekspresi sembarang ke Earth Engine.
fn valid_multipolygon(v: &Value) -> bool {
    let (mut polys, mut pts) = (0usize, 0usize);
    let Some(mp) = v.as_array() else { return false };
    for poly in mp {
        let Some(rings) = poly.as_array() else { return false };
        polys += 1;
        for ring in rings {
            let Some(ps) = ring.as_array() else { return false };
            if ps.len() < 4 {
                return false;
            }
            for p in ps {
                let Some(c) = p.as_array() else { return false };
                if c.len() != 2 {
                    return false;
                }
                let (Some(lo), Some(la)) = (c[0].as_f64(), c[1].as_f64()) else { return false };
                if !(90.0..=145.0).contains(&lo) || !(-15.0..=10.0).contains(&la) {
                    return false; // di luar kotak Indonesia
                }
                pts += 1;
            }
        }
    }
    polys > 0 && polys <= 2500 && pts <= 90_000
}

async fn zonal(s: AppState, kind: &'static str, body: ZonalBody) -> Result<Json<Value>, ApiErr> {
    if !valid_multipolygon(&body.coords) {
        return Err((StatusCode::BAD_REQUEST, "geometri tidak valid atau terlalu besar".into()));
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.coords.to_string().hash(&mut h);
    let ck = format!("{kind}:{:x}", h.finish());
    if let Some((t, v)) = s.zonal_cache.read().await.get(&ck) {
        if t.elapsed() < ZONAL_TTL {
            return Ok(Json(v.clone()));
        }
    }
    let val = gee::zonal(&s.client, kind, &body.coords, BURN_DAYS).await.map_err(|e| {
        eprintln!("GEE zonal error ({kind}): {e}");
        (StatusCode::BAD_GATEWAY, format!("Gagal menghitung {kind}"))
    })?;
    let v = json!({ "value": val });
    s.zonal_cache.write().await.insert(ck, (Instant::now(), v.clone()));
    Ok(Json(v))
}

async fn zonal_burned(State(s): State<AppState>, Json(b): Json<ZonalBody>) -> Result<Json<Value>, ApiErr> {
    zonal(s, "burned", b).await
}

async fn zonal_pop(State(s): State<AppState>, Json(b): Json<ZonalBody>) -> Result<Json<Value>, ApiErr> {
    zonal(s, "pop", b).await
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let key = std::env::var("FIRMS_MAP_KEY").expect("FIRMS_MAP_KEY belum diset di .env");
    let state = AppState {
        key,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(60)) // perhitungan zonal GEE bisa lebih lama dari FIRMS
            .build()
            .unwrap(),
        cache: Arc::new(RwLock::new(HashMap::new())),
        tile_cache: Arc::new(RwLock::new(HashMap::new())),
        zonal_cache: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/api/fires", get(fires))
        .route("/api/co-tiles", get(co_tiles))
        .route("/api/burned-tiles", get(burned_tiles))
        .route("/api/zonal/burned", post(zonal_burned))
        .route("/api/zonal/pop", post(zonal_pop))
        .route("/api/health", get(|| async { "ok" }))
        .layer(CorsLayer::permissive())
        .with_state(state);

    // ikuti port dari Vercel, 8080 hanya cadangan untuk lokal
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("API jalan di {addr}");
    axum::serve(listener, app).await.unwrap();
}