CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE streams (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'stopped',
    quality         TEXT NOT NULL DEFAULT 'hd',
    ingest_url      TEXT NOT NULL DEFAULT 'rtmp://a.rtmp.youtube.com/live2',
    stream_key      TEXT NOT NULL DEFAULT '',
    infinite        INTEGER NOT NULL DEFAULT 1,
    shuffle_video   INTEGER NOT NULL DEFAULT 0,
    shuffle_audio   INTEGER NOT NULL DEFAULT 1,
    overlay_enabled INTEGER NOT NULL DEFAULT 0,
    overlay_text    TEXT,                     -- optional; NULL or empty = no overlay
    fade            INTEGER NOT NULL DEFAULT 1,
    image_duration  INTEGER NOT NULL DEFAULT 10,
    scheduled_start INTEGER,
    scheduled_stop  INTEGER,
    last_error      TEXT,
    created_at      INTEGER NOT NULL
);

CREATE TABLE media (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    stream_id     INTEGER NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,            -- 'video' | 'image' | 'audio'
    original_name TEXT NOT NULL,
    stored_path   TEXT NOT NULL,            -- absolute path to the normalized file
    display_order INTEGER NOT NULL DEFAULT 0,
    duration_secs REAL,
    created_at    INTEGER NOT NULL
);

CREATE INDEX idx_media_stream ON media(stream_id, display_order);

CREATE TABLE youtube_accounts (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    channel_id    TEXT NOT NULL,
    channel_title TEXT NOT NULL,
    access_token  TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    expires_at    INTEGER NOT NULL
);
