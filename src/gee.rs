use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Membuat map GEE untuk komposit CO TROPOMI dan mengembalikan URL tile {z}/{x}/{y}.
pub async fn create_co_map(client: &reqwest::Client, days: i64) -> Result<String, String> {
    let token = access_token(client).await?;
    let project = std::env::var("GEE_PROJECT").map_err(|e| e.to_string())?;

    let end = chrono::Utc::now() + chrono::Duration::days(1); // filterDate end bersifat eksklusif
    let start = end - chrono::Duration::days(days + 1);
    let expr: Value = serde_json::from_str(
        &include_str!("co_expression.json")
            .replace("__START__", &start.format("%Y-%m-%d").to_string())
            .replace("__END__", &end.format("%Y-%m-%d").to_string()),
    )
    .map_err(|e| e.to_string())?;

    let res: Value = client
        .post(format!("https://earthengine.googleapis.com/v1/projects/{project}/maps"))
        .bearer_auth(token)
        .json(&json!({ "expression": expr, "fileFormat": "PNG" }))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    let name = res["name"].as_str().ok_or_else(|| format!("maps gagal: {res}"))?;
    Ok(format!("https://earthengine.googleapis.com/v1/{name}/tiles/{{z}}/{{x}}/{{y}}"))
}