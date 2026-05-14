//! ffmpeg command construction.
//!
//! Three stages:
//!   1. **Normalization** (on upload) \u{2014} every uploaded file is transcoded to a uniform
//!      format so it can be concatenated losslessly and reliably:
//!        - visual media -> 1080p / 30fps / yuv420p H.264 MP4 (audio stripped)
//!        - images       -> a fixed-length MP4 clip
//!        - audio        -> 44.1kHz stereo AAC (.m4a)
//!   2. **Round remux** (`round_remux_args`) \u{2014} a feeder concatenates one shuffled round
//!      of media (no re-encode) into a continuous MPEG-TS stream piped into a FIFO.
//!   3. **Broadcast** (`build_live_args`) \u{2014} one long-lived ffmpeg reads the video + audio
//!      FIFOs, re-encodes with YouTube-recommended settings, and pushes to RTMP. It is
//!      never restarted for media changes, so the stream is never interrupted.

use anyhow::{anyhow, Result};
use std::path::Path;
use tokio::process::Command;

use crate::models::Stream;

/// Scale + pad any input to a centered 1920x1080 frame without distortion.
const NORMALIZE_SCALE: &str =
    "scale=1920:1080:force_original_aspect_ratio=decrease,\
     pad=1920:1080:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1";

/// Run ffmpeg to completion, returning an error (with context) on non-zero exit.
pub async fn run(bin: &str, args: &[String], ctx: &str) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| anyhow!("could not launch ffmpeg ({ctx}): {e}"))?;
    if !status.success() {
        return Err(anyhow!("ffmpeg failed while {ctx} (exit {status})"));
    }
    Ok(())
}

/// Args to normalize a video or an image into a uniform MP4 clip.
pub fn normalize_visual_args(
    input: &Path,
    output: &Path,
    is_image: bool,
    image_duration: i64,
) -> Vec<String> {
    let vf = format!("{NORMALIZE_SCALE},fps=30,format=yuv420p");
    let mut a: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
    ];
    if is_image {
        a.extend([
            "-loop".into(),
            "1".into(),
            "-t".into(),
            image_duration.clamp(1, 3600).to_string(),
        ]);
    }
    a.extend([
        "-i".into(),
        input.display().to_string(),
        "-an".into(),
        "-vf".into(),
        vf,
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "20".into(),
        "-r".into(),
        "30".into(),
        "-g".into(),
        "60".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-movflags".into(),
        "+faststart".into(),
        output.display().to_string(),
    ]);
    a
}

/// Args to normalize an audio file into a uniform stereo AAC track.
pub fn normalize_audio_args(input: &Path, output: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        input.display().to_string(),
        "-vn".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-ar".into(),
        "44100".into(),
        "-ac".into(),
        "2".into(),
        output.display().to_string(),
    ]
}

/// Args for a feeder's per-round ffmpeg: concatenate one round's media (no re-encode)
/// and emit a single continuous MPEG-TS stream on stdout, which the feeder pumps into
/// the broadcast ffmpeg's FIFO.
pub fn round_remux_args(playlist: &Path, is_video: bool) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        playlist.display().to_string(),
        "-c".into(),
        "copy".into(),
    ];
    if is_video {
        // required to carry H.264 from MP4 into MPEG-TS
        a.extend(["-bsf:v".into(), "h264_mp4toannexb".into()]);
    }
    a.extend(["-f".into(), "mpegts".into(), "pipe:1".into()]);
    a
}

/// Escape arbitrary text for use inside an ffmpeg `drawtext` filter argument.
fn escape_drawtext(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ':' => out.push_str("\\:"),
            '%' => out.push_str("\\%"),
            // an apostrophe inside a quoted filter is painful; swap for a typographic one
            '\'' => out.push('\u{2019}'),
            _ => out.push(c),
        }
    }
    out
}

/// Build the broadcast ffmpeg argument list: read the video + audio FIFOs (each a
/// continuous MPEG-TS stream produced by a feeder), re-encode for YouTube, push to RTMP.
///
/// This process is long-lived and is never restarted for media changes — feeders simply
/// keep filling the FIFOs — so the stream is never interrupted.
pub fn build_live_args(s: &Stream, video_fifo: &Path, audio_fifo: &Path) -> Vec<String> {
    let hd = s.is_hd();

    // YouTube-recommended bitrates for 30fps.
    let (scale, vbitrate, maxrate, bufsize) = if hd {
        ("scale=1920:1080", "4500k", "4800k", "9000k")
    } else {
        ("scale=1280:720", "2500k", "2700k", "5400k")
    };

    let mut vf = scale.to_string();
    if s.fade {
        // a gentle fade-in when the stream comes up
        vf.push_str(",fade=t=in:st=0:d=2");
    }
    // The overlay text is optional: drawn only when enabled and non-empty.
    let overlay_text = s.overlay_text.as_deref().map(str::trim).unwrap_or("");
    if s.overlay_enabled && !overlay_text.is_empty() {
        let fontsize = if hd { 34 } else { 26 };
        vf.push_str(&format!(
            ",drawtext=font=Sans:text='{}':fontcolor=white:fontsize={}:\
             box=1:boxcolor=black@0.5:boxborderw=12:x=(w-text_w)/2:y=h-th-50",
            escape_drawtext(overlay_text),
            fontsize
        ));
    }

    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        // ---- input 0: video FIFO (continuous MPEG-TS from the video feeder) ----
        "-re".into(),
        "-fflags".into(),
        "+genpts".into(),
        "-i".into(),
        video_fifo.display().to_string(),
        // ---- input 1: audio FIFO ----
        "-re".into(),
        "-fflags".into(),
        "+genpts".into(),
        "-i".into(),
        audio_fifo.display().to_string(),
        // ---- mapping & filtering ----
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "1:a:0".into(),
        "-filter:v".into(),
        vf,
        // smooth over any timestamp gaps at feeder round boundaries
        "-af".into(),
        "aresample=async=1".into(),
        // ---- video encode (YouTube optimized) ----
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-profile:v".into(),
        "high".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-b:v".into(),
        vbitrate.into(),
        "-maxrate".into(),
        maxrate.into(),
        "-bufsize".into(),
        bufsize.into(),
        "-r".into(),
        "30".into(),
        "-g".into(),
        "60".into(),
        "-keyint_min".into(),
        "30".into(),
        "-sc_threshold".into(),
        "0".into(),
        // ---- audio encode ----
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "128k".into(),
        "-ar".into(),
        "44100".into(),
        "-ac".into(),
        "2".into(),
    ];

    if !s.infinite {
        // end the broadcast cleanly once a feeder finishes its single pass
        a.push("-shortest".into());
    }

    // machine-readable progress on stdout for the monitoring/metrics layer
    a.extend(["-nostats".into(), "-progress".into(), "pipe:1".into()]);

    a.extend(["-f".into(), "flv".into(), s.rtmp_target()]);
    a
}
