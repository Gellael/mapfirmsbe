use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

// Ekspresi Earth Engine (hasil scripts/gen_gee_expressions.py, di folder yang sama dengan file ini).
const CO_EXPR: &str = include_str!("co_expression.json");
const BURN_MAP: &str = include_str!("burn_map.json");
const BURN_ZONAL: &str = include_str!("burn_zonal.json");
const POP_ZONAL: &str = include_str!("pop_zonal.json");

#[derive(Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
}

#[derive(Serialize)]
struct Claims {
    iss: String,
    scope: String,
    aud: String,
    iat: u64,
    exp: u64,
}

async fn access_token(client: &reqwest::Client) -> Result<String, String> {
    let raw = std::env::var("GEE_SERVICE_ACCOUNT_JSON").map_err(|e| e.to_string())?;
    let sa: ServiceAccount = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let claims = Claims {
        iss: sa.client_email,
        scope: "https://www.googleapis.com/auth/earthengine".into(),
        aud: "https://oauth2.googleapis.com/token".into(),
        iat: now,
        exp: now + 3600,
    };
    let key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes()).map_err(|e| e.to_string())?;
    let jwt = encode(&Header::new(Algorithm::RS256), &claims, &key).map_err(|e| e.to_string())?;

    let res: Value = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", jwt.as_str()),
        ])
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    res["access_token"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| format!("token gagal: {res}"))
}

/// Mengisi __START__ / __END__ pada templat ekspresi. Mengembalikan (teks, start, end).
fn fill_dates(tpl: &str, days: i64) -> (String, String, String) {
    let end = chrono::Utc::now() + chrono::Duration::days(1); // filterDate end bersifat eksklusif
    let start = end - chrono::Duration::days(days + 1);
    let (s, e) = (start.format("%Y-%m-%d").to_string(), end.format("%Y-%m-%d").to_string());
    (tpl.replace("__START__", &s).replace("__END__", &e), s, e)
}

/// Membuat map GEE untuk lapisan raster ("co" atau "burned"). Mengembalikan (URL tile {z}/{x}/{y}, start, end).
pub async fn create_map(client: &reqwest::Client, kind: &str, days: i64) -> Result<(String, String, String), String> {
    let tpl = match kind {
        "co" => CO_EXPR,
        "burned" => BURN_MAP,
        _ => return Err(format!("jenis lapisan tidak dikenal: {kind}")),
    };
    let token = access_token(client).await?;
    let project = std::env::var("GEE_PROJECT").map_err(|e| e.to_string())?;
    let (txt, start, end) = fill_dates(tpl, days);
    let expr: Value = serde_json::from_str(&txt).map_err(|e| e.to_string())?;

    let res: Value = client
        .post(format!("https://earthengine.googleapis.com/v1/projects/{project}/maps"))
        .bearer_auth(token)
        .json(&json!({ "expression": expr, "fileFormat": "PNG" }))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    let name = res["name"].as_str().ok_or_else(|| format!("maps gagal: {res}"))?;
    Ok((format!("https://earthengine.googleapis.com/v1/{name}/tiles/{{z}}/{{x}}/{{y}}"), start, end))
}

/// Menjumlahkan nilai raster ("burned" = hektar terbakar, "pop" = jiwa) di dalam MultiPolygon.
/// `coords` adalah koordinat MultiPolygon GeoJSON (lon, lat) yang sudah divalidasi pemanggil.
pub async fn zonal(client: &reqwest::Client, kind: &str, coords: &Value, days: i64) -> Result<f64, String> {
    let tpl = match kind {
        "burned" => BURN_ZONAL,
        "pop" => POP_ZONAL,
        _ => return Err(format!("jenis tidak dikenal: {kind}")),
    };
    let token = access_token(client).await?;
    let project = std::env::var("GEE_PROJECT").map_err(|e| e.to_string())?;
    let (txt, _, _) = fill_dates(tpl, days);
    let txt = txt.replace("\"__COORDS__\"", &coords.to_string());
    let expr: Value = serde_json::from_str(&txt).map_err(|e| e.to_string())?;

    let res: Value = client
        .post(format!("https://earthengine.googleapis.com/v1/projects/{project}/value:compute"))
        .bearer_auth(token)
        .json(&json!({ "expression": expr }))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    let r = &res["result"];
    if let Some(v) = r.as_f64() {
        return Ok(v);
    }
    // jaga-jaga bila hasilnya berupa dictionary {band: nilai}
    if let Some(o) = r.as_object() {
        if let Some(v) = o.values().find_map(|x| x.as_f64()) {
            return Ok(v);
        }
    }
    if r.is_null() {
        return Ok(0.0); // tidak ada piksel valid di dalam geometri
    }
    Err(format!("compute gagal: {res}"))
}