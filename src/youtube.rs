//! Optional YouTube Data API v3 integration.
//!
//! When Google OAuth credentials are configured the user can connect their channel,
//! and infistreamer can auto-create a live broadcast + stream and fill in the RTMP
//! ingest URL / key. Without OAuth, the app still works fully in "stream key" mode.

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::models::YoutubeAccount;

const SCOPE: &str = "https://www.googleapis.com/auth/youtube";
const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/youtube/v3";

/// Minimal percent-encoding for OAuth query parameter values.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The Google consent-screen URL, or `None` if OAuth isn't configured.
pub fn auth_url(cfg: &Config) -> Option<String> {
    let cid = cfg.google_client_id.as_ref()?;
    let redirect = cfg.oauth_redirect_url.as_ref()?;
    Some(format!(
        "{AUTH_ENDPOINT}?client_id={}&redirect_uri={}&response_type=code\
         &scope={}&access_type=offline&prompt=consent",
        enc(cid),
        enc(redirect),
        enc(SCOPE),
    ))
}

struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

async fn token_request(params: &[(&str, &str)]) -> Result<TokenResponse> {
    let resp = reqwest::Client::new()
        .post(TOKEN_ENDPOINT)
        .form(params)
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        return Err(anyhow!("Google token endpoint error: {body}"));
    }
    Ok(TokenResponse {
        access_token: body["access_token"]
            .as_str()
            .context("token response missing access_token")?
            .to_string(),
        refresh_token: body["refresh_token"].as_str().map(str::to_string),
        expires_in: body["expires_in"].as_i64().unwrap_or(3600),
    })
}

/// Exchange the OAuth `code` for tokens, fetch the channel, and persist the account.
pub async fn connect(cfg: &Config, db: &SqlitePool, code: &str) -> Result<String> {
    let cid = cfg.google_client_id.as_deref().context("OAuth not configured")?;
    let secret = cfg
        .google_client_secret
        .as_deref()
        .context("OAuth not configured")?;
    let redirect = cfg
        .oauth_redirect_url
        .as_deref()
        .context("OAuth not configured")?;

    let tok = token_request(&[
        ("client_id", cid),
        ("client_secret", secret),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect),
    ])
    .await?;

    let refresh = tok.refresh_token.context(
        "Google did not return a refresh token \u{2014} revoke app access in your Google \
         account and connect again",
    )?;
    let (channel_id, channel_title) = get_channel(&tok.access_token).await?;
    let expires_at = chrono::Utc::now().timestamp() + tok.expires_in - 60;

    sqlx::query(
        "INSERT INTO youtube_accounts \
            (id, channel_id, channel_title, access_token, refresh_token, expires_at) \
         VALUES (1, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
            channel_id    = excluded.channel_id, \
            channel_title = excluded.channel_title, \
            access_token  = excluded.access_token, \
            refresh_token = excluded.refresh_token, \
            expires_at    = excluded.expires_at",
    )
    .bind(&channel_id)
    .bind(&channel_title)
    .bind(&tok.access_token)
    .bind(&refresh)
    .bind(expires_at)
    .execute(db)
    .await?;

    Ok(channel_title)
}

pub async fn account(db: &SqlitePool) -> Result<Option<YoutubeAccount>> {
    Ok(
        sqlx::query_as::<_, YoutubeAccount>("SELECT * FROM youtube_accounts WHERE id = 1")
            .fetch_optional(db)
            .await?,
    )
}

/// Return a non-expired access token, refreshing it if necessary.
pub async fn valid_token(cfg: &Config, db: &SqlitePool) -> Result<String> {
    let acc = account(db)
        .await?
        .context("no YouTube account connected")?;
    if acc.expires_at > chrono::Utc::now().timestamp() {
        return Ok(acc.access_token);
    }
    let cid = cfg.google_client_id.as_deref().context("OAuth not configured")?;
    let secret = cfg
        .google_client_secret
        .as_deref()
        .context("OAuth not configured")?;
    let tok = token_request(&[
        ("client_id", cid),
        ("client_secret", secret),
        ("refresh_token", &acc.refresh_token),
        ("grant_type", "refresh_token"),
    ])
    .await?;
    let expires_at = chrono::Utc::now().timestamp() + tok.expires_in - 60;
    sqlx::query("UPDATE youtube_accounts SET access_token = ?, expires_at = ? WHERE id = 1")
        .bind(&tok.access_token)
        .bind(expires_at)
        .execute(db)
        .await?;
    Ok(tok.access_token)
}

async fn get_channel(token: &str) -> Result<(String, String)> {
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{API}/channels?part=snippet&mine=true"))
        .bearer_auth(token)
        .send()
        .await?
        .json()
        .await?;
    let item = body["items"]
        .get(0)
        .context("no YouTube channel found for this Google account")?;
    Ok((
        item["id"].as_str().unwrap_or_default().to_string(),
        item["snippet"]["title"]
            .as_str()
            .unwrap_or("YouTube channel")
            .to_string(),
    ))
}

pub struct Ingestion {
    pub ingest_url: String,
    pub stream_key: String,
    pub watch_url: String,
}

/// Create a YouTube live broadcast + stream and bind them, returning ingest details.
pub async fn create_broadcast(
    token: &str,
    title: &str,
    privacy: &str,
    hd: bool,
) -> Result<Ingestion> {
    let client = reqwest::Client::new();

    // 1. create the live stream (the RTMP ingestion endpoint).
    let resolution = if hd { "1080p" } else { "720p" };
    let stream: serde_json::Value = client
        .post(format!("{API}/liveStreams?part=snippet,cdn,contentDetails"))
        .bearer_auth(token)
        .json(&json!({
            "snippet": { "title": title },
            "cdn": {
                "frameRate": "30fps",
                "ingestionType": "rtmp",
                "resolution": resolution
            },
            "contentDetails": { "isReusable": true }
        }))
        .send()
        .await?
        .json()
        .await?;
    let stream_id = stream["id"]
        .as_str()
        .with_context(|| format!("creating live stream failed: {stream}"))?
        .to_string();
    let ingest_url = stream["cdn"]["ingestionInfo"]["ingestionAddress"]
        .as_str()
        .context("YouTube response missing ingestion address")?
        .to_string();
    let stream_key = stream["cdn"]["ingestionInfo"]["streamName"]
        .as_str()
        .context("YouTube response missing stream key")?
        .to_string();

    // 2. create the broadcast (auto start/stop based on the ingest signal).
    let start = chrono::Utc::now() + chrono::Duration::minutes(1);
    let broadcast: serde_json::Value = client
        .post(format!(
            "{API}/liveBroadcasts?part=snippet,status,contentDetails"
        ))
        .bearer_auth(token)
        .json(&json!({
            "snippet": {
                "title": title,
                "scheduledStartTime": start.to_rfc3339()
            },
            "status": {
                "privacyStatus": privacy,
                "selfDeclaredMadeForKids": false
            },
            "contentDetails": {
                "enableAutoStart": true,
                "enableAutoStop": true
            }
        }))
        .send()
        .await?
        .json()
        .await?;
    let broadcast_id = broadcast["id"]
        .as_str()
        .with_context(|| format!("creating broadcast failed: {broadcast}"))?
        .to_string();

    // 3. bind the broadcast to the stream.
    client
        .post(format!(
            "{API}/liveBroadcasts/bind?id={broadcast_id}&streamId={stream_id}&part=id,contentDetails"
        ))
        .bearer_auth(token)
        .send()
        .await?;

    Ok(Ingestion {
        ingest_url,
        stream_key,
        watch_url: format!("https://youtube.com/watch?v={broadcast_id}"),
    })
}
