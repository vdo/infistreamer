use std::path::PathBuf;

/// Runtime configuration, sourced entirely from environment variables (or a `.env` file).
#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub data_dir: PathBuf,
    pub secret_key: Option<String>,
    pub admin_username: String,
    pub admin_password: String,
    pub ffmpeg_bin: String,
    pub default_ingest_url: String,
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
    pub oauth_redirect_url: Option<String>,
    pub max_upload_mb: usize,
}

impl Config {
    pub fn from_env() -> Self {
        fn opt(k: &str) -> Option<String> {
            std::env::var(k).ok().filter(|v| !v.trim().is_empty())
        }
        Config {
            bind_addr: opt("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into()),
            data_dir: PathBuf::from(opt("DATA_DIR").unwrap_or_else(|| "./data".into())),
            secret_key: opt("SECRET_KEY"),
            admin_username: opt("ADMIN_USERNAME").unwrap_or_else(|| "admin".into()),
            admin_password: opt("ADMIN_PASSWORD").unwrap_or_else(|| "admin".into()),
            ffmpeg_bin: opt("FFMPEG_BIN").unwrap_or_else(|| "ffmpeg".into()),
            default_ingest_url: opt("YOUTUBE_INGEST_URL")
                .unwrap_or_else(|| "rtmp://a.rtmp.youtube.com/live2".into()),
            google_client_id: opt("GOOGLE_CLIENT_ID"),
            google_client_secret: opt("GOOGLE_CLIENT_SECRET"),
            oauth_redirect_url: opt("OAUTH_REDIRECT_URL"),
            max_upload_mb: opt("MAX_UPLOAD_MB")
                .and_then(|v| v.parse().ok())
                .unwrap_or(4096),
        }
    }

    /// True when all Google OAuth settings are present, enabling the "connect account" flow.
    pub fn oauth_enabled(&self) -> bool {
        self.google_client_id.is_some()
            && self.google_client_secret.is_some()
            && self.oauth_redirect_url.is_some()
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("infistreamer.db")
    }
}
