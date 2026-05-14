//! HTTP layer: router, request handlers and Askama view models.

use std::collections::HashMap;

use askama::Template;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::PrivateCookieJar;
use serde::Deserialize;
use sqlx::SqlitePool;
use tokio::io::AsyncWriteExt;
use tower_http::services::ServeDir;

use crate::auth::{self, AuthUser};
use crate::error::AppError;
use crate::models::{Media, Stream, YoutubeAccount};
use crate::{ffmpeg, youtube, AppState};

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "bmp", "gif", "tif", "tiff", "heic", "avif",
];

pub fn router(state: AppState) -> Router {
    // Body-size cap scoped to the media upload route only (other routes keep axum's
    // small default). Uploads stream straight to disk, so large files never buffer in
    // memory. MAX_UPLOAD_MB=0 disables the limit entirely.
    let upload_limit = match state.config.max_upload_mb {
        0 => DefaultBodyLimit::disable(),
        mb => DefaultBodyLimit::max(mb.saturating_mul(1024 * 1024)),
    };

    Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/", get(dashboard))
        .route("/streams", post(create_stream))
        .route("/streams/:id", get(stream_detail).post(update_stream))
        .route("/streams/:id/delete", post(delete_stream))
        .route("/streams/:id/start", post(start_stream))
        .route("/streams/:id/stop", post(stop_stream))
        .route("/streams/:id/status", get(stream_status))
        .route(
            "/streams/:id/media",
            post(upload_media).layer(upload_limit),
        )
        .route("/streams/:id/media/:mid/delete", post(delete_media))
        .route("/streams/:id/media/:mid/move", post(move_media))
        .route("/streams/:id/youtube-broadcast", post(youtube_broadcast))
        .route("/monitoring", get(monitoring))
        .route("/monitoring/table", get(monitoring_table))
        .route("/monitoring/trends", get(monitoring_trends))
        .route("/youtube/connect", get(youtube_connect))
        .route("/youtube/callback", get(youtube_callback))
        .route("/youtube/disconnect", post(youtube_disconnect))
        .route("/metrics", get(metrics))
        .route("/healthz", get(|| async { "ok" }))
        .nest_service("/static", ServeDir::new("static"))
        // Serves normalized media for in-browser previews. Files live under random
        // UUIDs; the app is meant to run on localhost or behind Tailscale.
        .nest_service(
            "/media",
            ServeDir::new(state.config.data_dir.join("media")),
        )
        .with_state(state)
}

// ----------------------------------------------------------------------------
// View models
// ----------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    username: String,
    streams: Vec<StreamCard>,
    youtube: Option<YoutubeAccount>,
    oauth_enabled: bool,
    flash: Option<String>,
    flash_error: Option<String>,
}

struct StreamCard {
    stream: Stream,
    visual_count: i64,
    audio_count: i64,
}

#[derive(Template)]
#[template(path = "stream.html")]
struct StreamTemplate {
    username: String,
    stream: Stream,
    videos: Vec<Media>,
    images: Vec<Media>,
    audios: Vec<Media>,
    runtime: Option<RuntimeView>,
    youtube: Option<YoutubeAccount>,
    oauth_enabled: bool,
    sched_start: String,
    sched_stop: String,
    flash: Option<String>,
    flash_error: Option<String>,
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusTemplate {
    stream: Stream,
    runtime: Option<RuntimeView>,
}

/// Per-stream live numbers shown on the detail + status views.
struct RuntimeView {
    uptime: String,
    data_sent: String,
    bitrate: String,
    fps: String,
    frames: u64,
    dropped: u64,
    speed: String,
    restarts: u32,
}

#[derive(Template)]
#[template(path = "monitoring.html")]
struct MonitoringTemplate {
    username: String,
    rows: Vec<MonRow>,
    trends: Vec<TrendCard>,
}

#[derive(Template)]
#[template(path = "mon_table.html")]
struct MonTableTemplate {
    rows: Vec<MonRow>,
}

#[derive(Template)]
#[template(path = "trends.html")]
struct MonTrendsTemplate {
    trends: Vec<TrendCard>,
}

/// One stream's bitrate sparklines for the monitoring page. The `spark_*` fields hold
/// pre-rendered SVG markup (emitted with the `safe` filter in the template).
struct TrendCard {
    id: i64,
    name: String,
    is_live: bool,
    spark_24h: String,
    spark_1w: String,
    peak_24h: String,
    peak_1w: String,
}

struct MonRow {
    id: i64,
    name: String,
    status: String,
    status_emoji: String,
    is_live: bool,
    uptime: String,
    data_sent: String,
    bitrate: String,
    fps: String,
    frames: u64,
    dropped: u64,
    speed: String,
    restarts: u32,
}

// ----------------------------------------------------------------------------
// Auth handlers
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_page() -> Result<Response, AppError> {
    Ok(Html(LoginTemplate { error: None }.render()?).into_response())
}

async fn login_submit(
    State(st): State<AppState>,
    jar: PrivateCookieJar,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    let user = sqlx::query_as::<_, crate::models::User>(
        "SELECT * FROM users WHERE username = ?",
    )
    .bind(form.username.trim())
    .fetch_optional(&st.db)
    .await?;

    match user {
        Some(u) if auth::verify_password(&form.password, &u.password_hash) => {
            tracing::info!("user '{}' signed in", u.username);
            let jar = jar.add(auth::session_cookie(u.id));
            Ok((jar, Redirect::to("/")).into_response())
        }
        _ => {
            let html = LoginTemplate {
                error: Some("Invalid username or password".into()),
            }
            .render()?;
            Ok(Html(html).into_response())
        }
    }
}

async fn logout(jar: PrivateCookieJar) -> impl IntoResponse {
    (jar.remove(auth::cleared_cookie()), Redirect::to("/login"))
}

// ----------------------------------------------------------------------------
// Dashboard + stream CRUD
// ----------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct MediaCount {
    stream_id: i64,
    kind: String,
    c: i64,
}

#[derive(Deserialize)]
struct DashQuery {
    yt_ok: Option<String>,
    yt_error: Option<String>,
}

async fn dashboard(
    auth: AuthUser,
    State(st): State<AppState>,
    Query(dq): Query<DashQuery>,
) -> Result<Response, AppError> {
    let streams = sqlx::query_as::<_, Stream>(
        "SELECT * FROM streams ORDER BY created_at DESC, id DESC",
    )
    .fetch_all(&st.db)
    .await?;

    let count_rows = sqlx::query_as::<_, MediaCount>(
        "SELECT stream_id, kind, COUNT(*) AS c FROM media GROUP BY stream_id, kind",
    )
    .fetch_all(&st.db)
    .await?;
    let mut counts: HashMap<i64, (i64, i64)> = HashMap::new();
    for r in count_rows {
        let entry = counts.entry(r.stream_id).or_default();
        if r.kind == "audio" {
            entry.1 += r.c;
        } else {
            entry.0 += r.c;
        }
    }

    let cards = streams
        .into_iter()
        .map(|s| {
            let (v, a) = counts.get(&s.id).copied().unwrap_or((0, 0));
            StreamCard {
                stream: s,
                visual_count: v,
                audio_count: a,
            }
        })
        .collect();

    let youtube = youtube::account(&st.db).await.ok().flatten();
    let html = DashboardTemplate {
        username: auth.username,
        streams: cards,
        youtube,
        oauth_enabled: st.config.oauth_enabled(),
        flash: dq.yt_ok,
        flash_error: dq.yt_error,
    }
    .render()?;
    Ok(Html(html).into_response())
}

async fn create_stream(
    _auth: AuthUser,
    State(st): State<AppState>,
    Form(f): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    let name = f
        .get("name")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("stream name is required".into()))?;

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO streams (name, ingest_url, created_at) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&st.config.default_ingest_url)
    .bind(chrono::Utc::now().timestamp())
    .fetch_one(&st.db)
    .await?;

    tracing::info!("stream {id} created: '{name}'");
    Ok(Redirect::to(&format!("/streams/{id}")))
}

#[derive(Deserialize)]
struct DetailQuery {
    msg: Option<String>,
    err: Option<String>,
}

async fn stream_detail(
    auth: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<DetailQuery>,
) -> Result<Response, AppError> {
    let stream = load_stream(&st.db, id).await?;
    let media = sqlx::query_as::<_, Media>(
        "SELECT * FROM media WHERE stream_id = ? ORDER BY display_order, id",
    )
    .bind(id)
    .fetch_all(&st.db)
    .await?;

    let videos = media.iter().filter(|m| m.kind == "video").cloned().collect();
    let images = media.iter().filter(|m| m.kind == "image").cloned().collect();
    let audios = media.iter().filter(|m| m.is_audio()).cloned().collect();

    let runtime = runtime_view(&st, id).await;
    let youtube = youtube::account(&st.db).await.ok().flatten();

    let html = StreamTemplate {
        username: auth.username,
        sched_start: fmt_dt(stream.scheduled_start),
        sched_stop: fmt_dt(stream.scheduled_stop),
        stream,
        videos,
        images,
        audios,
        runtime,
        youtube,
        oauth_enabled: st.config.oauth_enabled(),
        flash: q.msg,
        flash_error: q.err,
    }
    .render()?;
    Ok(Html(html).into_response())
}

async fn update_stream(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    load_stream(&st.db, id).await?; // 404 if missing

    let name = f
        .get("name")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("stream name is required".into()))?;
    let quality = if f.get("quality").map(String::as_str) == Some("hd") {
        "hd"
    } else {
        "sd"
    };
    let ingest_url = f
        .get("ingest_url")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("rtmp://a.rtmp.youtube.com/live2");
    let stream_key = f.get("stream_key").map(|s| s.trim()).unwrap_or("");
    // overlay text is optional \u{2014} store NULL when blank
    let overlay_text: Option<&str> = f
        .get("overlay_text")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let image_duration: i64 = f
        .get("image_duration")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(10)
        .clamp(1, 3600);

    sqlx::query(
        "UPDATE streams SET \
            name = ?, quality = ?, ingest_url = ?, stream_key = ?, \
            infinite = ?, shuffle_video = ?, shuffle_audio = ?, \
            overlay_enabled = ?, overlay_text = ?, fade = ?, \
            image_duration = ?, scheduled_start = ?, scheduled_stop = ? \
         WHERE id = ?",
    )
    .bind(name)
    .bind(quality)
    .bind(ingest_url)
    .bind(stream_key)
    .bind(checkbox(&f, "infinite"))
    .bind(checkbox(&f, "shuffle_video"))
    .bind(checkbox(&f, "shuffle_audio"))
    .bind(checkbox(&f, "overlay_enabled"))
    .bind(overlay_text)
    .bind(checkbox(&f, "fade"))
    .bind(image_duration)
    .bind(parse_dt(f.get("scheduled_start")))
    .bind(parse_dt(f.get("scheduled_stop")))
    .bind(id)
    .execute(&st.db)
    .await?;

    Ok(Redirect::to(&format!(
        "/streams/{id}?msg={}",
        urlencode("Settings saved")
    )))
}

async fn delete_stream(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Redirect, AppError> {
    let _ = st.manager.stop(id).await;
    let _ = tokio::fs::remove_dir_all(st.config.data_dir.join("media").join(id.to_string())).await;
    let _ = tokio::fs::remove_dir_all(st.config.data_dir.join("work").join(id.to_string())).await;
    sqlx::query("DELETE FROM streams WHERE id = ?")
        .bind(id)
        .execute(&st.db)
        .await?;
    tracing::info!("stream {id} deleted");
    Ok(Redirect::to("/"))
}

// ----------------------------------------------------------------------------
// Stream control
// ----------------------------------------------------------------------------

async fn start_stream(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Redirect, AppError> {
    if let Err(e) = st.manager.start(id).await {
        let _ = sqlx::query("UPDATE streams SET last_error = ? WHERE id = ?")
            .bind(e.to_string())
            .bind(id)
            .execute(&st.db)
            .await;
        return Ok(Redirect::to(&format!(
            "/streams/{id}?err={}",
            urlencode(&e.to_string())
        )));
    }
    Ok(Redirect::to(&format!("/streams/{id}")))
}

async fn stop_stream(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Redirect, AppError> {
    st.manager.stop(id).await.map_err(AppError::Other)?;
    Ok(Redirect::to(&format!("/streams/{id}")))
}

async fn stream_status(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let stream = load_stream(&st.db, id).await?;
    let runtime = runtime_view(&st, id).await;
    let html = StatusTemplate { stream, runtime }.render()?;
    Ok(Html(html).into_response())
}

// ----------------------------------------------------------------------------
// Media upload / management
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct UploadQuery {
    /// "visual" or "audio"
    kind: String,
}

async fn upload_media(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<Redirect, AppError> {
    let stream = load_stream(&st.db, id).await?;
    let is_audio_upload = q.kind == "audio";

    let media_dir = st.config.data_dir.join("media").join(id.to_string());
    tokio::fs::create_dir_all(&media_dir).await?;

    // The form's file input allows multiple selection, so a single request may carry
    // many "file" parts. Process them one at a time: stream to a temp file, normalize,
    // insert. Failures are collected so one bad file doesn't sink the whole batch.
    let mut next_order: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(display_order), 0) + 1 FROM media WHERE stream_id = ?",
    )
    .bind(id)
    .fetch_one(&st.db)
    .await?;

    let mut saved = 0usize;
    let mut errors: Vec<String> = Vec::new();

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("upload error: {e}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let original = field.file_name().unwrap_or("upload").to_string();
        // a `<input multiple>` with nothing selected still sends an empty part
        if original.is_empty() {
            continue;
        }
        let ext = std::path::Path::new(&original)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin")
            .to_lowercase();

        // stream this file straight to a temp file
        let tmp_path = st
            .config
            .data_dir
            .join("tmp")
            .join(format!("{}.{}", uuid::Uuid::new_v4(), ext));
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut empty = true;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| AppError::BadRequest(format!("upload error: {e}")))?
        {
            empty = false;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        if empty {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            continue;
        }

        let is_image = !is_audio_upload && IMAGE_EXTS.contains(&ext.as_str());
        let uid = uuid::Uuid::new_v4();
        let (out_path, kind, args) = if is_audio_upload {
            let out = media_dir.join(format!("{uid}.m4a"));
            let args = ffmpeg::normalize_audio_args(&tmp_path, &out);
            (out, "audio", args)
        } else {
            let out = media_dir.join(format!("{uid}.mp4"));
            let args =
                ffmpeg::normalize_visual_args(&tmp_path, &out, is_image, stream.image_duration);
            (out, if is_image { "image" } else { "video" }, args)
        };

        let normalize = ffmpeg::run(&st.config.ffmpeg_bin, &args, "normalizing upload").await;
        let _ = tokio::fs::remove_file(&tmp_path).await;
        if let Err(e) = normalize {
            let _ = tokio::fs::remove_file(&out_path).await;
            errors.push(format!("'{original}': {e}"));
            continue;
        }

        sqlx::query(
            "INSERT INTO media \
                (stream_id, kind, original_name, stored_path, display_order, duration_secs, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(kind)
        .bind(&original)
        .bind(out_path.display().to_string())
        .bind(next_order)
        .bind(if is_image {
            Some(stream.image_duration as f64)
        } else {
            None
        })
        .bind(chrono::Utc::now().timestamp())
        .execute(&st.db)
        .await?;
        next_order += 1;
        saved += 1;
    }

    if saved == 0 && errors.is_empty() {
        return Err(AppError::BadRequest("no file received".into()));
    }
    if saved > 0 || !errors.is_empty() {
        tracing::info!(
            "stream {id}: media upload — {saved} added, {} failed",
            errors.len()
        );
    }

    // Apply the new media to the stream if it is currently live.
    let live = if saved > 0 {
        if let Err(e) = st.manager.refresh(id).await {
            tracing::warn!("stream {id}: refresh after upload failed: {e}");
        }
        st.manager.is_running(id).await
    } else {
        false
    };

    if !errors.is_empty() {
        let msg = format!(
            "{saved} file(s) added, {} failed \u{2014} {}",
            errors.len(),
            errors.join("; ")
        );
        return Ok(Redirect::to(&format!("/streams/{id}?err={}", urlencode(&msg))));
    }
    let mut msg = format!("{saved} file(s) added");
    if live {
        msg.push_str(" \u{2014} added to the live rotation (no interruption)");
    }
    Ok(Redirect::to(&format!("/streams/{id}?msg={}", urlencode(&msg))))
}

async fn delete_media(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path((id, mid)): Path<(i64, i64)>,
) -> Result<Redirect, AppError> {
    let m = sqlx::query_as::<_, Media>("SELECT * FROM media WHERE id = ? AND stream_id = ?")
        .bind(mid)
        .bind(id)
        .fetch_optional(&st.db)
        .await?;
    if let Some(m) = m {
        let _ = tokio::fs::remove_file(&m.stored_path).await;
        sqlx::query("DELETE FROM media WHERE id = ?")
            .bind(mid)
            .execute(&st.db)
            .await?;
        if let Err(e) = st.manager.refresh(id).await {
            tracing::warn!("stream {id}: refresh after media delete failed: {e}");
        }
    }
    Ok(Redirect::to(&format!("/streams/{id}")))
}

#[derive(Deserialize)]
struct MoveForm {
    dir: String,
}

async fn move_media(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path((id, mid)): Path<(i64, i64)>,
    Form(f): Form<MoveForm>,
) -> Result<Redirect, AppError> {
    let all = sqlx::query_as::<_, Media>(
        "SELECT * FROM media WHERE stream_id = ? ORDER BY display_order, id",
    )
    .bind(id)
    .fetch_all(&st.db)
    .await?;

    let Some(target) = all.iter().find(|m| m.id == mid) else {
        return Ok(Redirect::to(&format!("/streams/{id}")));
    };
    // reorder within the same group (visual media share one ordering, audio another)
    let group: Vec<&Media> = all
        .iter()
        .filter(|m| m.is_audio() == target.is_audio())
        .collect();
    let pos = group.iter().position(|m| m.id == mid).unwrap();

    let other = match f.dir.as_str() {
        "up" if pos > 0 => Some(group[pos - 1]),
        "down" if pos + 1 < group.len() => Some(group[pos + 1]),
        _ => None,
    };

    if let Some(other) = other {
        // swap display_order values
        sqlx::query("UPDATE media SET display_order = ? WHERE id = ?")
            .bind(other.display_order)
            .bind(target.id)
            .execute(&st.db)
            .await?;
        sqlx::query("UPDATE media SET display_order = ? WHERE id = ?")
            .bind(target.display_order)
            .bind(other.id)
            .execute(&st.db)
            .await?;
        if let Err(e) = st.manager.refresh(id).await {
            tracing::warn!("stream {id}: refresh after reorder failed: {e}");
        }
    }
    Ok(Redirect::to(&format!("/streams/{id}")))
}

// ----------------------------------------------------------------------------
// Monitoring + Prometheus metrics
// ----------------------------------------------------------------------------

async fn monitoring_rows(st: &AppState) -> Result<Vec<MonRow>, AppError> {
    let streams = sqlx::query_as::<_, Stream>("SELECT * FROM streams ORDER BY name")
        .fetch_all(&st.db)
        .await?;
    let runtime: HashMap<i64, _> = st.manager.all_runtime().await.into_iter().collect();
    let now = chrono::Utc::now().timestamp();

    Ok(streams
        .into_iter()
        .map(|s| {
            let rt = runtime.get(&s.id);
            MonRow {
                status_emoji: s.status_emoji().to_string(),
                is_live: s.is_live(),
                uptime: rt.map(|r| fmt_uptime(now - r.started_at)).unwrap_or_else(|| "\u{2014}".into()),
                data_sent: rt.map(|r| fmt_bytes(r.bytes_sent)).unwrap_or_else(|| "\u{2014}".into()),
                bitrate: rt.map(|r| format!("{:.0} kbps", r.bitrate_kbps)).unwrap_or_else(|| "\u{2014}".into()),
                fps: rt.map(|r| format!("{:.1}", r.fps)).unwrap_or_else(|| "\u{2014}".into()),
                frames: rt.map(|r| r.frames).unwrap_or(0),
                dropped: rt.map(|r| r.dropped).unwrap_or(0),
                speed: rt.map(|r| format!("{:.2}x", r.speed)).unwrap_or_else(|| "\u{2014}".into()),
                restarts: rt.map(|r| r.restarts).unwrap_or(0),
                id: s.id,
                name: s.name,
                status: s.status,
            }
        })
        .collect())
}

async fn monitoring(auth: AuthUser, State(st): State<AppState>) -> Result<Response, AppError> {
    let rows = monitoring_rows(&st).await?;
    let trends = trend_cards(&st).await?;
    let html = MonitoringTemplate {
        username: auth.username,
        rows,
        trends,
    }
    .render()?;
    Ok(Html(html).into_response())
}

async fn monitoring_table(
    _auth: AuthUser,
    State(st): State<AppState>,
) -> Result<Response, AppError> {
    let rows = monitoring_rows(&st).await?;
    let html = MonTableTemplate { rows }.render()?;
    Ok(Html(html).into_response())
}

async fn monitoring_trends(
    _auth: AuthUser,
    State(st): State<AppState>,
) -> Result<Response, AppError> {
    let trends = trend_cards(&st).await?;
    let html = MonTrendsTemplate { trends }.render()?;
    Ok(Html(html).into_response())
}

#[derive(sqlx::FromRow)]
struct SamplePoint {
    ts: i64,
    bitrate_kbps: f64,
}

/// Build the per-stream 24h and 1-week bitrate sparkline cards.
async fn trend_cards(st: &AppState) -> Result<Vec<TrendCard>, AppError> {
    let streams = sqlx::query_as::<_, Stream>("SELECT * FROM streams ORDER BY name")
        .fetch_all(&st.db)
        .await?;
    let now = chrono::Utc::now().timestamp();
    let mut cards = Vec::with_capacity(streams.len());

    for s in streams {
        let day = trend_series(&st.db, s.id, now - 86_400, now, 48).await?;
        let week = trend_series(&st.db, s.id, now - 604_800, now, 84).await?;
        let peak = |series: &[f64]| series.iter().copied().fold(0.0_f64, f64::max);
        cards.push(TrendCard {
            id: s.id,
            is_live: s.is_live(),
            name: s.name,
            peak_24h: format!("{:.0} kbps", peak(&day)),
            peak_1w: format!("{:.0} kbps", peak(&week)),
            spark_24h: sparkline(&day, 260.0, 40.0),
            spark_1w: sparkline(&week, 260.0, 40.0),
        });
    }
    Ok(cards)
}

/// Fetch metric samples in `[start, end]` and bucket their bitrate into `buckets` points.
async fn trend_series(
    db: &SqlitePool,
    stream_id: i64,
    start: i64,
    end: i64,
    buckets: usize,
) -> Result<Vec<f64>, AppError> {
    let rows = sqlx::query_as::<_, SamplePoint>(
        "SELECT ts, bitrate_kbps FROM metric_samples \
         WHERE stream_id = ? AND ts >= ? ORDER BY ts",
    )
    .bind(stream_id)
    .bind(start)
    .fetch_all(db)
    .await?;
    let points: Vec<(i64, f64)> = rows.into_iter().map(|r| (r.ts, r.bitrate_kbps)).collect();
    Ok(bucketize(&points, start, end, buckets))
}

/// Average the samples falling in each of `buckets` equal time slices across `[start, end]`.
fn bucketize(samples: &[(i64, f64)], start: i64, end: i64, buckets: usize) -> Vec<f64> {
    let buckets = buckets.max(1);
    let span = (end - start).max(1) as f64;
    let mut sums = vec![0.0_f64; buckets];
    let mut counts = vec![0u32; buckets];
    for &(ts, v) in samples {
        let frac = ((ts - start) as f64 / span).clamp(0.0, 0.999_999);
        let b = ((frac * buckets as f64) as usize).min(buckets - 1);
        sums[b] += v;
        counts[b] += 1;
    }
    sums.iter()
        .zip(counts)
        .map(|(s, c)| if c > 0 { s / c as f64 } else { 0.0 })
        .collect()
}

/// Render a small SVG sparkline (area + line) from a series of values.
fn sparkline(values: &[f64], width: f64, height: f64) -> String {
    let pad = 3.0;
    if values.iter().all(|v| *v <= 0.0) {
        // nothing recorded yet (or stream never live in range): a flat baseline
        return format!(
            "<svg class=\"spark\" viewBox=\"0 0 {w:.0} {h:.0}\" preserveAspectRatio=\"none\">\
             <line class=\"spark-base\" x1=\"0\" y1=\"{b:.1}\" x2=\"{w:.0}\" y2=\"{b:.1}\" \
             vector-effect=\"non-scaling-stroke\"/></svg>",
            w = width,
            h = height,
            b = height - pad
        );
    }
    let max = values.iter().copied().fold(0.0_f64, f64::max).max(1.0);
    let n = values.len();
    let plot_h = height - pad * 2.0;
    let step = if n > 1 { width / (n as f64 - 1.0) } else { width };
    let mut pts = String::new();
    for (i, v) in values.iter().enumerate() {
        let x = i as f64 * step;
        let y = pad + plot_h - (v / max).clamp(0.0, 1.0) * plot_h;
        if i > 0 {
            pts.push(' ');
        }
        pts.push_str(&format!("{x:.1},{y:.1}"));
    }
    let last_x = (n as f64 - 1.0).max(0.0) * step;
    format!(
        "<svg class=\"spark\" viewBox=\"0 0 {w:.0} {h:.0}\" preserveAspectRatio=\"none\">\
         <path class=\"spark-area\" d=\"M0,{h:.0} L{pts} L{lx:.1},{h:.0} Z\"/>\
         <polyline class=\"spark-line\" points=\"{pts}\" vector-effect=\"non-scaling-stroke\"/>\
         </svg>",
        w = width,
        h = height,
        pts = pts,
        lx = last_x
    )
}

/// Prometheus exposition. Intentionally unauthenticated so a local scraper can read it.
async fn metrics(State(st): State<AppState>) -> Result<Response, AppError> {
    let streams = sqlx::query_as::<_, Stream>("SELECT id, name, status, quality, ingest_url, stream_key, infinite, shuffle_video, shuffle_audio, overlay_enabled, overlay_text, fade, image_duration, scheduled_start, scheduled_stop, last_error, created_at FROM streams")
        .fetch_all(&st.db)
        .await?;
    let runtime: HashMap<i64, _> = st.manager.all_runtime().await.into_iter().collect();
    let now = chrono::Utc::now().timestamp();

    let mut b = String::new();
    let live_count = streams.iter().filter(|s| s.is_live()).count();
    b.push_str("# HELP infistreamer_streams_total Configured streams.\n");
    b.push_str("# TYPE infistreamer_streams_total gauge\n");
    b.push_str(&format!("infistreamer_streams_total {}\n", streams.len()));
    b.push_str("# HELP infistreamer_streams_live Streams currently live.\n");
    b.push_str("# TYPE infistreamer_streams_live gauge\n");
    b.push_str(&format!("infistreamer_streams_live {live_count}\n"));

    b.push_str("# HELP infistreamer_stream_up Stream is live (1) or not (0).\n");
    b.push_str("# TYPE infistreamer_stream_up gauge\n");
    b.push_str("# HELP infistreamer_stream_bytes_sent_total Bytes pushed to the RTMP ingest.\n");
    b.push_str("# TYPE infistreamer_stream_bytes_sent_total counter\n");
    b.push_str("# HELP infistreamer_stream_uptime_seconds Seconds since the stream started.\n");
    b.push_str("# TYPE infistreamer_stream_uptime_seconds gauge\n");
    b.push_str("# HELP infistreamer_stream_bitrate_kbps Current outgoing video+audio bitrate.\n");
    b.push_str("# TYPE infistreamer_stream_bitrate_kbps gauge\n");
    b.push_str("# HELP infistreamer_stream_fps Current encoding frames per second.\n");
    b.push_str("# TYPE infistreamer_stream_fps gauge\n");
    b.push_str("# HELP infistreamer_stream_frames_total Frames encoded by the current ffmpeg process.\n");
    b.push_str("# TYPE infistreamer_stream_frames_total counter\n");
    b.push_str("# HELP infistreamer_stream_dropped_frames_total Frames dropped by ffmpeg.\n");
    b.push_str("# TYPE infistreamer_stream_dropped_frames_total counter\n");
    b.push_str("# HELP infistreamer_stream_speed_ratio ffmpeg encode speed (1.0 = realtime).\n");
    b.push_str("# TYPE infistreamer_stream_speed_ratio gauge\n");
    b.push_str("# HELP infistreamer_stream_restarts_total Watchdog restarts since the stream started.\n");
    b.push_str("# TYPE infistreamer_stream_restarts_total counter\n");

    for s in &streams {
        let labels = format!(
            "{{stream_id=\"{}\",name=\"{}\"}}",
            s.id,
            prom_escape(&s.name)
        );
        let rt = runtime.get(&s.id);
        let up = if rt.is_some() { 1 } else { 0 };
        b.push_str(&format!("infistreamer_stream_up{labels} {up}\n"));
        if let Some(r) = rt {
            b.push_str(&format!(
                "infistreamer_stream_bytes_sent_total{labels} {}\n",
                r.bytes_sent
            ));
            b.push_str(&format!(
                "infistreamer_stream_uptime_seconds{labels} {}\n",
                (now - r.started_at).max(0)
            ));
            b.push_str(&format!(
                "infistreamer_stream_bitrate_kbps{labels} {:.1}\n",
                r.bitrate_kbps
            ));
            b.push_str(&format!("infistreamer_stream_fps{labels} {:.2}\n", r.fps));
            b.push_str(&format!(
                "infistreamer_stream_frames_total{labels} {}\n",
                r.frames
            ));
            b.push_str(&format!(
                "infistreamer_stream_dropped_frames_total{labels} {}\n",
                r.dropped
            ));
            b.push_str(&format!(
                "infistreamer_stream_speed_ratio{labels} {:.3}\n",
                r.speed
            ));
            b.push_str(&format!(
                "infistreamer_stream_restarts_total{labels} {}\n",
                r.restarts
            ));
        }
    }

    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        b,
    )
        .into_response())
}

// ----------------------------------------------------------------------------
// YouTube integration
// ----------------------------------------------------------------------------

async fn youtube_connect(_auth: AuthUser, State(st): State<AppState>) -> Result<Redirect, AppError> {
    let url = youtube::auth_url(&st.config)
        .ok_or_else(|| AppError::BadRequest("YouTube OAuth is not configured".into()))?;
    Ok(Redirect::to(&url))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    error: Option<String>,
}

async fn youtube_callback(
    _auth: AuthUser,
    State(st): State<AppState>,
    Query(q): Query<CallbackQuery>,
) -> Result<Redirect, AppError> {
    if let Some(err) = q.error {
        return Ok(Redirect::to(&format!("/?yt_error={}", urlencode(&err))));
    }
    let code = q
        .code
        .ok_or_else(|| AppError::BadRequest("missing authorization code".into()))?;
    match youtube::connect(&st.config, &st.db, &code).await {
        Ok(title) => {
            tracing::info!("YouTube account connected: {title}");
            Ok(Redirect::to(&format!(
                "/?yt_ok={}",
                urlencode(&format!("Connected: {title}"))
            )))
        }
        Err(e) => Ok(Redirect::to(&format!("/?yt_error={}", urlencode(&e.to_string())))),
    }
}

async fn youtube_disconnect(
    _auth: AuthUser,
    State(st): State<AppState>,
) -> Result<Redirect, AppError> {
    sqlx::query("DELETE FROM youtube_accounts WHERE id = 1")
        .execute(&st.db)
        .await?;
    Ok(Redirect::to("/"))
}

async fn youtube_broadcast(
    _auth: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Redirect, AppError> {
    let stream = load_stream(&st.db, id).await?;
    let result = async {
        let token = youtube::valid_token(&st.config, &st.db).await?;
        youtube::create_broadcast(&token, &stream.name, "unlisted", stream.is_hd()).await
    }
    .await;

    match result {
        Ok(ing) => {
            sqlx::query("UPDATE streams SET ingest_url = ?, stream_key = ? WHERE id = ?")
                .bind(&ing.ingest_url)
                .bind(&ing.stream_key)
                .bind(id)
                .execute(&st.db)
                .await?;
            Ok(Redirect::to(&format!(
                "/streams/{id}?msg={}",
                urlencode(&format!(
                    "YouTube broadcast created (unlisted). Watch: {}",
                    ing.watch_url
                ))
            )))
        }
        Err(e) => Ok(Redirect::to(&format!(
            "/streams/{id}?err={}",
            urlencode(&e.to_string())
        ))),
    }
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

async fn load_stream(db: &SqlitePool, id: i64) -> Result<Stream, AppError> {
    sqlx::query_as::<_, Stream>("SELECT * FROM streams WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::Sqlx(sqlx::Error::RowNotFound))
}

async fn runtime_view(st: &AppState, id: i64) -> Option<RuntimeView> {
    let r = st.manager.runtime_info(id).await?;
    let now = chrono::Utc::now().timestamp();
    Some(RuntimeView {
        uptime: fmt_uptime(now - r.started_at),
        data_sent: fmt_bytes(r.bytes_sent),
        bitrate: format!("{:.0} kbps", r.bitrate_kbps),
        fps: format!("{:.1}", r.fps),
        frames: r.frames,
        dropped: r.dropped,
        speed: format!("{:.2}x", r.speed),
        restarts: r.restarts,
    })
}

fn checkbox(f: &HashMap<String, String>, key: &str) -> bool {
    matches!(
        f.get(key).map(String::as_str),
        Some("on") | Some("true") | Some("1")
    )
}

/// Parse an HTML `datetime-local` value into a UTC unix timestamp.
fn parse_dt(v: Option<&String>) -> Option<i64> {
    let s = v?.trim();
    if s.is_empty() {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

/// Format a UTC unix timestamp back into a `datetime-local` input value.
fn fmt_dt(ts: Option<i64>) -> String {
    match ts {
        Some(t) => chrono::DateTime::from_timestamp(t, 0)
            .map(|d| d.format("%Y-%m-%dT%H:%M").to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

fn fmt_uptime(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m {sec:02}s")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Minimal percent-encoding for redirect query values.
fn urlencode(s: &str) -> String {
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

/// Escape a Prometheus label value.
fn prom_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
