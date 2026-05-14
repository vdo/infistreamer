mod auth;
mod config;
mod db;
mod error;
mod ffmpeg;
mod history;
mod models;
mod scheduler;
mod supervisor;
mod web;
mod youtube;

use std::sync::Arc;

use anyhow::Result;
use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;

use config::Config;
use supervisor::StreamManager;

/// Shared application state, cloned into every request handler.
#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::SqlitePool,
    pub key: Key,
    pub config: Arc<Config>,
    pub manager: StreamManager,
}

// Lets `PrivateCookieJar` pull the signing key out of `AppState`.
impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.key.clone()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,infistreamer=debug".into()),
        )
        .init();

    let config = Config::from_env();
    tracing::info!(
        "starting infistreamer — data dir {}, ffmpeg '{}', uploads up to {} MB, YouTube OAuth {}",
        config.data_dir.display(),
        config.ffmpeg_bin,
        config.max_upload_mb,
        if config.oauth_enabled() { "enabled" } else { "disabled (stream-key mode)" },
    );

    // Ensure the data directory layout exists.
    tokio::fs::create_dir_all(&config.data_dir).await?;
    tokio::fs::create_dir_all(config.data_dir.join("media")).await?;
    tokio::fs::create_dir_all(config.data_dir.join("work")).await?;
    tokio::fs::create_dir_all(config.data_dir.join("tmp")).await?;

    let db = db::init(&config.db_path()).await?;
    db::ensure_admin(&db, &config.admin_username, &config.admin_password).await?;
    tracing::info!("database ready at {}", config.db_path().display());

    // Cookie signing key: from SECRET_KEY (>= 64 bytes) or ephemeral.
    let key = match &config.secret_key {
        Some(s) if s.len() >= 64 => Key::from(s.as_bytes()),
        Some(_) => {
            tracing::warn!("SECRET_KEY is shorter than 64 bytes; using an ephemeral key instead");
            Key::generate()
        }
        None => {
            tracing::warn!("SECRET_KEY not set; sessions will not survive a restart");
            Key::generate()
        }
    };

    let config = Arc::new(config);
    let manager = StreamManager::new(db.clone(), config.clone());
    manager.reset_stale().await?;
    scheduler::spawn(db.clone(), manager.clone());
    history::spawn(db.clone(), manager.clone());
    tracing::info!("scheduler and metrics sampler started");

    let state = AppState {
        db,
        key,
        config: config.clone(),
        manager,
    };

    let app = web::router(state);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!("infistreamer listening on http://{}", config.bind_addr);
    axum::serve(listener, app).await?;
    Ok(())
}
