mod db;
mod ui;
mod whatsapp;

use std::{env, net::SocketAddr, path::Path, sync::Arc};

use anyhow::{Context, bail};
use axum::{Router, extract::DefaultBodyLimit, routing::get};
use reqwest::Client;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::SqlitePool,
    pub config: Arc<Config>,
    pub http: Client,
}

pub struct Config {
    pub app_password: String,
    pub session_secret: Vec<u8>,
    pub secure_cookie: bool,
    pub bind: String,
    pub database_path: String,
    pub wa_access_token: Option<String>,
    pub wa_phone_number_id: Option<String>,
    pub wa_app_secret: Option<String>,
    pub wa_verify_token: Option<String>,
    pub graph_api_version: String,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let app_password = required("APP_PASSWORD")?;
        if app_password.len() < 12 {
            bail!("APP_PASSWORD must contain at least 12 characters");
        }
        if app_password == "change-this-to-a-long-unique-password" {
            bail!("replace the APP_PASSWORD example value before starting the server");
        }

        let session_secret = required("SESSION_SECRET")?;
        if session_secret.len() < 32 {
            bail!("SESSION_SECRET must contain at least 32 bytes");
        }
        if session_secret == "replace-with-at-least-32-random-bytes" {
            bail!("replace the SESSION_SECRET example value before starting the server");
        }
        let session_secret = session_secret.into_bytes();

        let graph_api_version = env::var("GRAPH_API_VERSION").unwrap_or_else(|_| "v23.0".into());
        let valid_version = graph_api_version
            .strip_prefix('v')
            .is_some_and(|version| {
                let mut parts = version.split('.');
                matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None)
                    if !a.is_empty() && !b.is_empty() && a.bytes().all(|c| c.is_ascii_digit()) && b.bytes().all(|c| c.is_ascii_digit()))
            });
        if !valid_version {
            bail!("GRAPH_API_VERSION must look like v23.0");
        }

        let wa_phone_number_id = optional("WHATSAPP_PHONE_NUMBER_ID");
        if wa_phone_number_id
            .as_deref()
            .is_some_and(|value| !value.bytes().all(|byte| byte.is_ascii_digit()))
        {
            bail!("WHATSAPP_PHONE_NUMBER_ID must contain only digits");
        }

        Ok(Self {
            app_password,
            session_secret,
            secure_cookie: env::var("COOKIE_SECURE")
                .map(|value| value.eq_ignore_ascii_case("true"))
                .unwrap_or(true),
            bind: env::var("APP_BIND")
                .or_else(|_| env::var("PORT").map(|port| format!("0.0.0.0:{port}")))
                .unwrap_or_else(|_| "127.0.0.1:3000".into()),
            database_path: env::var("DATABASE_PATH")
                .unwrap_or_else(|_| "data/whatsapp.sqlite".into()),
            wa_access_token: optional("WHATSAPP_ACCESS_TOKEN"),
            wa_phone_number_id,
            wa_app_secret: optional("WHATSAPP_APP_SECRET"),
            wa_verify_token: optional("WHATSAPP_VERIFY_TOKEN"),
            graph_api_version,
        })
    }

    pub fn can_send(&self) -> bool {
        self.wa_access_token.is_some() && self.wa_phone_number_id.is_some()
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| format!("required environment variable {name} is missing"))
}

fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let config = Arc::new(Config::from_env()?);

    if let Some(parent) = Path::new(&config.database_path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("could not create database directory {}", parent.display())
            })?;
        }
    }

    let options = SqliteConnectOptions::new()
        .filename(&config.database_path)
        .create_if_missing(true)
        .foreign_keys(true);
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .context("could not open the SQLite database")?;
    db::initialize(&db).await?;

    let state = AppState {
        db,
        config: config.clone(),
        http: Client::builder()
            .user_agent(concat!("crm-bling/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not initialize the HTTPS client")?,
    };

    let app = Router::new()
        .route("/", get(ui::home))
        .route("/login", axum::routing::post(ui::login))
        .route("/logout", axum::routing::post(ui::logout))
        .route("/contacts", axum::routing::post(ui::save_contact))
        .route("/messages", axum::routing::post(ui::send_message))
        .route("/messages/template", axum::routing::post(ui::send_template))
        .route(
            "/webhook",
            get(whatsapp::verify_webhook).post(whatsapp::receive_webhook),
        )
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(1_048_576))
        .with_state(state);

    let address: SocketAddr = config
        .bind
        .parse()
        .with_context(|| format!("APP_BIND is not a valid socket address: {}", config.bind))?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("could not listen on {address}"))?;
    println!("CRM-BLING listening on http://{address}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("HTTP server stopped unexpectedly")
}
