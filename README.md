# 📡 infistreamer

Self-hosted 24/7 YouTube livestreamer. Give it a playlist of videos & images plus a
playlist of audio tracks and it builds a continuous livestream with ffmpeg — managed from
a simple web UI.

## Features

- Web UI with user/password login; manage multiple independent streams
- Upload media from the browser (multiple files at once, with progress) — visual media
  and audio are kept separate
- 1080p or 720p output, ffmpeg tuned for YouTube's recommended settings
- **Never-stop architecture** — a long-lived broadcast ffmpeg per stream fed by feeder
  threads; media can be added/removed/reordered while live with no interruption
- "Bag" shuffle, infinite looping, text overlay, fade-in, scheduled start/stop
- Auto-restart watchdog, monitoring tab (uptime, data sent, bitrate, fps…) with 24h / 1w
  graphs, and Prometheus metrics at `/metrics`
- YouTube integration: works with a manual stream key, or connect a Google account
  (OAuth) to auto-create broadcasts
- One-command installer (Docker + Compose on any major Linux, optional Tailscale)

## Quick start

```bash
git clone git@github.com:vdo/infistreamer.git && cd infistreamer
./install.sh          # installs Docker/Compose, writes .env, builds & starts
```

Or manually:

```bash
cp .env.example .env  # then edit it — set ADMIN_PASSWORD and SECRET_KEY
docker compose up -d --build
```

Open `http://localhost:8080` and log in with the credentials from `.env`.

## Streaming to YouTube

- **Stream key:** in YouTube Studio → Go Live, copy the stream key into the stream's
  settings, then click *Go live*.
- **OAuth (optional):** set `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` /
  `OAUTH_REDIRECT_URL` in `.env` (YouTube Data API v3 enabled), connect your account on
  the dashboard, then use *Auto-create YouTube broadcast* on a stream.

## Configuration

All configuration is environment variables — see [`.env.example`](.env.example).
Data (SQLite DB + normalized media) lives in `./data`.

## Development

```bash
cargo run     # needs ffmpeg on PATH
cargo test
```

Built with Rust · Axum · Askama + htmx · SQLite (sqlx) · ffmpeg.
