// Model structs mirror DB rows for `FromRow`; not every column is read in code.
#![allow(dead_code)]

use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct Stream {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub quality: String,
    pub ingest_url: String,
    pub stream_key: String,
    pub infinite: bool,
    pub shuffle_video: bool,
    pub shuffle_audio: bool,
    pub overlay_enabled: bool,
    pub overlay_text: Option<String>,
    pub fade: bool,
    pub image_duration: i64,
    pub scheduled_start: Option<i64>,
    pub scheduled_stop: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
}

impl Stream {
    pub fn is_hd(&self) -> bool {
        self.quality == "hd"
    }
    pub fn quality_label(&self) -> &'static str {
        if self.is_hd() {
            "1080p HD"
        } else {
            "720p"
        }
    }
    pub fn status_emoji(&self) -> &'static str {
        match self.status.as_str() {
            "live" => "\u{1F534}",     // red circle
            "error" => "\u{26A0}",     // warning
            "starting" => "\u{23F3}",  // hourglass
            _ => "\u{26AA}",           // white circle
        }
    }
    pub fn is_live(&self) -> bool {
        self.status == "live"
    }
    /// The RTMP target ffmpeg streams to.
    pub fn rtmp_target(&self) -> String {
        format!(
            "{}/{}",
            self.ingest_url.trim_end_matches('/'),
            self.stream_key.trim()
        )
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Media {
    pub id: i64,
    pub stream_id: i64,
    pub kind: String,
    pub original_name: String,
    pub stored_path: String,
    pub display_order: i64,
    pub duration_secs: Option<f64>,
    pub created_at: i64,
}

impl Media {
    pub fn kind_emoji(&self) -> &'static str {
        match self.kind.as_str() {
            "video" => "\u{1F3AC}", // clapper
            "image" => "\u{1F5BC}", // framed picture
            "audio" => "\u{1F3B5}", // musical note
            _ => "\u{1F4C4}",
        }
    }
    pub fn is_audio(&self) -> bool {
        self.kind == "audio"
    }
    /// Basename of the normalized file on disk, used to build its `/media/...` URL.
    pub fn file_name(&self) -> &str {
        std::path::Path::new(&self.stored_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct YoutubeAccount {
    pub id: i64,
    pub channel_id: String,
    pub channel_title: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}
