//! Stream supervisor: keeps each stream's broadcast ffmpeg alive forever.
//!
//! Architecture:
//!   * One long-lived **broadcast ffmpeg** per stream reads two FIFOs (video + audio,
//!     each a continuous MPEG-TS stream) and pushes to RTMP.
//!   * Two **feeder threads** continuously concatenate the stream's current media into
//!     those FIFOs, one shuffled "round" at a time, re-reading the media list every
//!     round.
//!   * Adding / removing / reordering media just swaps the shared media list. The
//!     feeders pick it up on their next round — the broadcast ffmpeg is **never touched**,
//!     so the stream is never interrupted.
//!   * A **watchdog** restarts the broadcast ffmpeg (and its feeders) only if it actually
//!     crashes.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use rand::seq::SliceRandom;
use sqlx::SqlitePool;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::{ffmpeg, models};

/// Delay between a broadcast-ffmpeg crash and the watchdog restarting it.
const RESTART_DELAY_SECS: u64 = 5;

/// Live metrics for one running stream, updated lock-free from ffmpeg progress output.
#[derive(Default)]
struct Metrics {
    /// Bytes pushed by ffmpeg processes that have already exited (survives restarts).
    bytes_base: AtomicU64,
    /// Bytes pushed by the currently running ffmpeg process.
    bytes_current: AtomicU64,
    bitrate_milli: AtomicU64, // kbps * 1000
    fps_milli: AtomicU64,     // fps * 1000
    frames: AtomicU64,
    dropped: AtomicU64,
    speed_milli: AtomicU64, // speed ratio * 1000
}

/// A point-in-time snapshot of a running stream, handed to the UI / metrics layer.
#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    pub started_at: i64,
    pub restarts: u32,
    pub bytes_sent: u64,
    pub bitrate_kbps: f64,
    pub fps: f64,
    pub frames: u64,
    pub dropped: u64,
    pub speed: f64,
}

/// The current media (file paths, in display order) for a running stream's feeders.
/// Updated by `refresh()`; read by the feeder threads at the start of every round.
type FeedList = Arc<StdMutex<Vec<String>>>;

struct RunningStream {
    stop: Arc<Notify>,
    video_list: FeedList,
    audio_list: FeedList,
    restarts: Arc<AtomicU32>,
    metrics: Arc<Metrics>,
    started_at: i64,
    _handle: JoinHandle<()>,
}

impl RunningStream {
    fn snapshot(&self) -> RuntimeInfo {
        let m = &self.metrics;
        RuntimeInfo {
            started_at: self.started_at,
            restarts: self.restarts.load(Ordering::Relaxed),
            bytes_sent: m.bytes_base.load(Ordering::Relaxed)
                + m.bytes_current.load(Ordering::Relaxed),
            bitrate_kbps: m.bitrate_milli.load(Ordering::Relaxed) as f64 / 1000.0,
            fps: m.fps_milli.load(Ordering::Relaxed) as f64 / 1000.0,
            frames: m.frames.load(Ordering::Relaxed),
            dropped: m.dropped.load(Ordering::Relaxed),
            speed: m.speed_milli.load(Ordering::Relaxed) as f64 / 1000.0,
        }
    }
}

#[derive(Clone)]
pub struct StreamManager {
    db: SqlitePool,
    config: Arc<Config>,
    running: Arc<Mutex<HashMap<i64, RunningStream>>>,
}

impl StreamManager {
    pub fn new(db: SqlitePool, config: Arc<Config>) -> Self {
        Self {
            db,
            config,
            running: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn is_running(&self, id: i64) -> bool {
        self.running.lock().await.contains_key(&id)
    }

    pub async fn runtime_info(&self, id: i64) -> Option<RuntimeInfo> {
        self.running.lock().await.get(&id).map(RunningStream::snapshot)
    }

    /// Snapshot of every currently running stream, keyed by stream id.
    pub async fn all_runtime(&self) -> Vec<(i64, RuntimeInfo)> {
        self.running
            .lock()
            .await
            .iter()
            .map(|(id, rs)| (*id, rs.snapshot()))
            .collect()
    }

    /// On boot, no ffmpeg processes exist — clear any stale "live" rows.
    pub async fn reset_stale(&self) -> Result<()> {
        sqlx::query(
            "UPDATE streams SET status = 'stopped' \
             WHERE status IN ('live', 'starting', 'error')",
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Start streaming: create FIFOs, spawn the broadcast ffmpeg + feeder threads, and a
    /// watchdog that keeps the broadcast ffmpeg alive.
    pub async fn start(&self, id: i64) -> Result<()> {
        if self.running.lock().await.contains_key(&id) {
            return Ok(());
        }

        let stream = sqlx::query_as::<_, models::Stream>("SELECT * FROM streams WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| anyhow!("stream not found"))?;

        if stream.stream_key.trim().is_empty() {
            return Err(anyhow!(
                "no stream key set — paste one from YouTube Studio or connect a YouTube account"
            ));
        }

        let (videos, audios) = load_media_lists(&self.db, id).await?;
        if videos.is_empty() {
            return Err(anyhow!("add at least one video or image first"));
        }
        if audios.is_empty() {
            return Err(anyhow!("add at least one audio track first"));
        }
        let (visual_count, audio_count) = (videos.len(), audios.len());

        let workdir = self.config.data_dir.join("work").join(id.to_string());
        tokio::fs::create_dir_all(&workdir).await?;

        // Per-run unique paths so a quick stop+start can't have the old monitor's cleanup
        // race with the new one.
        let run = uuid::Uuid::new_v4();
        let video_fifo = workdir.join(format!("{run}.video.fifo"));
        let audio_fifo = workdir.join(format!("{run}.audio.fifo"));
        let video_playlist = workdir.join(format!("{run}.video.txt"));
        let audio_playlist = workdir.join(format!("{run}.audio.txt"));

        let video_list: FeedList = Arc::new(StdMutex::new(videos));
        let audio_list: FeedList = Arc::new(StdMutex::new(audios));
        let args = ffmpeg::build_live_args(&stream, &video_fifo, &audio_fifo);

        let stop = Arc::new(Notify::new());
        let restarts = Arc::new(AtomicU32::new(0));
        let metrics = Arc::new(Metrics::default());
        let started_at = chrono::Utc::now().timestamp();

        let ctx = MonitorCtx {
            db: self.db.clone(),
            ffmpeg_bin: self.config.ffmpeg_bin.clone(),
            args,
            video_fifo,
            audio_fifo,
            video_playlist,
            audio_playlist,
            stop: stop.clone(),
            restarts: restarts.clone(),
            metrics: metrics.clone(),
            video_list: video_list.clone(),
            audio_list: audio_list.clone(),
            shuffle_video: stream.shuffle_video,
            shuffle_audio: stream.shuffle_audio,
            infinite: stream.infinite,
            id,
            running: self.running.clone(),
        };
        let handle = tokio::spawn(run_monitor(ctx));

        self.running.lock().await.insert(
            id,
            RunningStream {
                stop,
                video_list,
                audio_list,
                restarts,
                metrics,
                started_at,
                _handle: handle,
            },
        );

        sqlx::query("UPDATE streams SET status = 'live', last_error = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;
        tracing::info!(
            "stream {id} '{}' started — {} quality, {visual_count} visual / {audio_count} audio, {}",
            stream.name,
            stream.quality_label(),
            if stream.infinite { "infinite loop" } else { "single pass" },
        );
        Ok(())
    }

    /// Stop a running stream. Idempotent.
    pub async fn stop(&self, id: i64) -> Result<()> {
        let entry = self.running.lock().await.remove(&id);
        if let Some(rs) = entry {
            rs.stop.notify_one();
            tracing::info!("stream {id} stopping");
        }
        sqlx::query("UPDATE streams SET status = 'stopped' WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Apply a media change (add / remove / reorder) to a running stream. This just swaps
    /// the shared media list — the feeder threads pick it up on their next round, with
    /// **zero interruption** to the broadcast ffmpeg. No-op when the stream isn't running.
    pub async fn refresh(&self, id: i64) -> Result<()> {
        let (video_list, audio_list) = {
            let running = self.running.lock().await;
            match running.get(&id) {
                Some(rs) => (rs.video_list.clone(), rs.audio_list.clone()),
                None => return Ok(()),
            }
        };
        let (videos, audios) = load_media_lists(&self.db, id).await?;
        let (visual_count, audio_count) = (videos.len(), audios.len());
        // Don't blank a list if all media of that kind was removed — keep the feeder
        // playing the previous round rather than stalling the stream.
        if !videos.is_empty() {
            *video_list.lock().unwrap() = videos;
        }
        if !audios.is_empty() {
            *audio_list.lock().unwrap() = audios;
        }
        tracing::info!(
            "stream {id}: media list updated ({visual_count} visual / {audio_count} audio) \
             — feeders apply it on the next round"
        );
        Ok(())
    }
}

/// Load a stream's media file paths, split into (visual, audio), in display order.
async fn load_media_lists(db: &SqlitePool, id: i64) -> Result<(Vec<String>, Vec<String>)> {
    let media = sqlx::query_as::<_, models::Media>(
        "SELECT * FROM media WHERE stream_id = ? ORDER BY display_order, id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    let videos = media
        .iter()
        .filter(|m| !m.is_audio())
        .map(|m| m.stored_path.clone())
        .collect();
    let audios = media
        .iter()
        .filter(|m| m.is_audio())
        .map(|m| m.stored_path.clone())
        .collect();
    Ok((videos, audios))
}

struct MonitorCtx {
    db: SqlitePool,
    ffmpeg_bin: String,
    args: Vec<String>,
    video_fifo: PathBuf,
    audio_fifo: PathBuf,
    video_playlist: PathBuf,
    audio_playlist: PathBuf,
    stop: Arc<Notify>,
    restarts: Arc<AtomicU32>,
    metrics: Arc<Metrics>,
    video_list: FeedList,
    audio_list: FeedList,
    shuffle_video: bool,
    shuffle_audio: bool,
    infinite: bool,
    id: i64,
    running: Arc<Mutex<HashMap<i64, RunningStream>>>,
}

/// The watchdog loop: keep the broadcast ffmpeg + its two feeders alive.
async fn run_monitor(ctx: MonitorCtx) {
    let MonitorCtx {
        db,
        ffmpeg_bin,
        args,
        video_fifo,
        audio_fifo,
        video_playlist,
        audio_playlist,
        stop,
        restarts,
        metrics,
        video_list,
        audio_list,
        shuffle_video,
        shuffle_audio,
        infinite,
        id,
        running,
    } = ctx;

    loop {
        // (Re)create the FIFOs fresh for this run.
        if let Err(e) = make_fifo(&video_fifo).and_then(|_| make_fifo(&audio_fifo)) {
            tracing::error!("stream {id}: could not create FIFOs: {e}");
            set_status(&db, id, "error", Some(&format!("FIFO error: {e}"))).await;
            tokio::select! {
                _ = stop.notified() => break,
                _ = tokio::time::sleep(Duration::from_secs(RESTART_DELAY_SECS)) => continue,
            }
        }

        // Spawn the broadcast ffmpeg (reads the two FIFOs -> RTMP).
        let spawn_result = Command::new(&ffmpeg_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawn_result {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("stream {id}: failed to launch ffmpeg: {e}");
                set_status(&db, id, "error", Some(&format!("could not launch ffmpeg: {e}")))
                    .await;
                tokio::select! {
                    _ = stop.notified() => break,
                    _ = tokio::time::sleep(Duration::from_secs(RESTART_DELAY_SECS)) => continue,
                }
            }
        };

        // Drain ffmpeg's progress output into the metrics struct.
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(read_progress(stdout, metrics.clone()));
        }
        tracing::info!("stream {id}: broadcast ffmpeg running, feeders starting");

        // Spawn the two feeder threads for this run. They are detached: a fresh
        // `feeder_stop` is used per run, and they also die on EPIPE once the broadcast
        // ffmpeg is gone, so old feeders never interfere with a new run.
        let feeder_stop = Arc::new(AtomicBool::new(false));
        spawn_feeder(FeederCtx {
            ffmpeg_bin: ffmpeg_bin.clone(),
            playlist: video_playlist.clone(),
            fifo: video_fifo.clone(),
            list: video_list.clone(),
            shuffle: shuffle_video,
            infinite,
            is_video: true,
            id,
            stop: feeder_stop.clone(),
        });
        spawn_feeder(FeederCtx {
            ffmpeg_bin: ffmpeg_bin.clone(),
            playlist: audio_playlist.clone(),
            fifo: audio_fifo.clone(),
            list: audio_list.clone(),
            shuffle: shuffle_audio,
            infinite,
            is_video: false,
            id,
            stop: feeder_stop.clone(),
        });

        tokio::select! {
            wait = child.wait() => {
                feeder_stop.store(true, Ordering::SeqCst);
                // roll the current process's byte count into the persistent base
                let current = metrics.bytes_current.swap(0, Ordering::Relaxed);
                metrics.bytes_base.fetch_add(current, Ordering::Relaxed);

                match wait {
                    Ok(status) if status.success() && !infinite => {
                        tracing::info!("stream {id}: finished");
                        set_status(&db, id, "stopped", None).await;
                        break;
                    }
                    Ok(status) => {
                        let n = restarts.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::warn!("stream {id}: broadcast ffmpeg exited ({status}); restart #{n}");
                        set_status(&db, id, "error",
                            Some(&format!("ffmpeg exited unexpectedly ({status}); restarting"))).await;
                    }
                    Err(e) => {
                        let n = restarts.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::warn!("stream {id}: error waiting on ffmpeg: {e}; restart #{n}");
                        set_status(&db, id, "error", Some(&format!("{e}"))).await;
                    }
                }

                tokio::select! {
                    _ = stop.notified() => break,
                    _ = tokio::time::sleep(Duration::from_secs(RESTART_DELAY_SECS)) => {}
                }
                set_status(&db, id, "live", None).await;
            }
            _ = stop.notified() => {
                feeder_stop.store(true, Ordering::SeqCst);
                let _ = child.kill().await;
                set_status(&db, id, "stopped", None).await;
                break;
            }
        }
    }

    // Best-effort cleanup of this run's FIFOs and playlists.
    for p in [&video_fifo, &audio_fifo, &video_playlist, &audio_playlist] {
        let _ = std::fs::remove_file(p);
    }
    running.lock().await.remove(&id);
}

/// Parse ffmpeg's `-progress` key=value stream and update the shared metrics.
async fn read_progress(stdout: ChildStdout, metrics: Arc<Metrics>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "total_size" => {
                if let Ok(n) = value.parse::<u64>() {
                    metrics.bytes_current.store(n, Ordering::Relaxed);
                }
            }
            "frame" => {
                if let Ok(n) = value.parse::<u64>() {
                    metrics.frames.store(n, Ordering::Relaxed);
                }
            }
            "drop_frames" => {
                if let Ok(n) = value.parse::<u64>() {
                    metrics.dropped.store(n, Ordering::Relaxed);
                }
            }
            "fps" => {
                if let Ok(f) = value.parse::<f64>() {
                    metrics.fps_milli.store((f * 1000.0) as u64, Ordering::Relaxed);
                }
            }
            "bitrate" => {
                let num = value.trim_end_matches("kbits/s").trim();
                if let Ok(f) = num.parse::<f64>() {
                    metrics
                        .bitrate_milli
                        .store((f * 1000.0) as u64, Ordering::Relaxed);
                }
            }
            "speed" => {
                let num = value.trim_end_matches('x').trim();
                if let Ok(f) = num.parse::<f64>() {
                    metrics
                        .speed_milli
                        .store((f * 1000.0) as u64, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
}

async fn set_status(db: &SqlitePool, id: i64, status: &str, err: Option<&str>) {
    let _ = sqlx::query("UPDATE streams SET status = ?, last_error = ? WHERE id = ?")
        .bind(status)
        .bind(err)
        .bind(id)
        .execute(db)
        .await;
}

// ----------------------------------------------------------------------------
// Feeders
// ----------------------------------------------------------------------------

struct FeederCtx {
    ffmpeg_bin: String,
    playlist: PathBuf,
    fifo: PathBuf,
    list: FeedList,
    shuffle: bool,
    infinite: bool,
    is_video: bool,
    id: i64,
    stop: Arc<AtomicBool>,
}

/// Spawn a feeder on its own blocking thread.
fn spawn_feeder(ctx: FeederCtx) {
    std::thread::spawn(move || {
        let kind = if ctx.is_video { "video" } else { "audio" };
        if let Err(e) = run_feeder(&ctx) {
            tracing::debug!("stream {} {kind} feeder ended: {e}", ctx.id);
        }
    });
}

/// A feeder: hold the FIFO open and pump shuffled rounds of media into it forever.
fn run_feeder(ctx: &FeederCtx) -> io::Result<()> {
    let mut fifo = match open_fifo_write(&ctx.fifo, &ctx.stop)? {
        Some(f) => f,
        None => return Ok(()), // asked to stop before the broadcast ffmpeg appeared
    };
    let mut last: Option<String> = None;

    loop {
        if ctx.stop.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Snapshot the current media list (a media change is just a swap of this).
        let mut files = ctx.list.lock().unwrap().clone();
        if files.is_empty() {
            if !ctx.infinite {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        if ctx.shuffle {
            shuffle_round(&mut files, last.as_deref());
        }

        write_playlist_blocking(&ctx.playlist, &files)?;

        // One ffmpeg per round: concat the round (no re-encode) -> MPEG-TS on stdout,
        // which we copy into the FIFO. `io::copy` lasts the whole round and blocks on the
        // FIFO (the broadcast ffmpeg's `-re` provides backpressure / pacing).
        let mut child = std::process::Command::new(&ctx.ffmpeg_bin)
            .args(ffmpeg::round_remux_args(&ctx.playlist, ctx.is_video))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut out = child.stdout.take().expect("piped stdout");
        let copy_result = io::copy(&mut out, &mut fifo);
        drop(out); // unblock the round ffmpeg if it was mid-write, so wait() returns
        let _ = child.wait();

        if let Err(e) = copy_result {
            // a write error means the broadcast ffmpeg is gone — this feeder is done
            return Err(e);
        }
        last = files.last().cloned();

        if !ctx.infinite {
            return Ok(()); // finite stream: fed the playlist once
        }
    }
}

/// Open a FIFO for writing, retrying until the broadcast ffmpeg opens the read end.
/// Returns `Ok(None)` if asked to stop before that happens.
fn open_fifo_write(path: &Path, stop: &Arc<AtomicBool>) -> io::Result<Option<std::fs::File>> {
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                // Clear O_NONBLOCK so subsequent writes block — that backpressure is how
                // the feeder is paced by the broadcast ffmpeg's `-re`.
                unsafe {
                    let fd = file.as_raw_fd();
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
                    }
                }
                return Ok(Some(file));
            }
            // ENXIO: write-only open with no reader yet — wait for the broadcast ffmpeg.
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Create a fresh FIFO at `path` (removing any stale one first).
fn make_fifo(path: &Path) -> io::Result<()> {
    let _ = std::fs::remove_file(path);
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad FIFO path"))?;
    let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Synchronous `concat` demuxer playlist writer (the async one lives in `ffmpeg`, but
/// feeders run on blocking threads). Writes via a temp file + rename.
fn write_playlist_blocking(path: &Path, files: &[String]) -> io::Result<()> {
    let mut body = String::new();
    for f in files {
        let escaped = f.replace('\'', "'\\''");
        body.push_str(&format!("file '{escaped}'\n"));
    }
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// One bag-shuffle round: a random permutation that avoids repeating `last` at the head,
/// so no track plays twice in a row across round boundaries.
fn shuffle_round(items: &mut [String], last: Option<&str>) {
    if items.len() <= 1 {
        return;
    }
    let mut rng = rand::thread_rng();
    for _ in 0..32 {
        items.shuffle(&mut rng);
        if last.map_or(true, |l| items[0] != l) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shuffle_round;

    fn items(n: usize) -> Vec<String> {
        (0..n).map(|i| i.to_string()).collect()
    }

    #[test]
    fn shuffle_round_is_a_permutation() {
        let mut v = items(7);
        shuffle_round(&mut v, None);
        let mut sorted = v.clone();
        sorted.sort();
        assert_eq!(sorted, items(7), "a round must contain every track exactly once");
    }

    #[test]
    fn shuffle_round_avoids_repeating_last() {
        for _ in 0..200 {
            let mut v = items(5);
            shuffle_round(&mut v, Some("3"));
            assert_ne!(v[0], "3", "a round must not start with the previous round's last track");
        }
    }

    #[test]
    fn shuffle_round_handles_trivial_inputs() {
        let mut one = vec!["x".to_string()];
        shuffle_round(&mut one, Some("x")); // unavoidable repeat — must not hang or panic
        assert_eq!(one, vec!["x".to_string()]);
        let mut empty: Vec<String> = vec![];
        shuffle_round(&mut empty, None);
        assert!(empty.is_empty());
    }
}
