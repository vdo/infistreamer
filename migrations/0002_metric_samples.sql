-- Time-series samples for the monitoring graphs. One row per stream per sample tick.
CREATE TABLE metric_samples (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    stream_id    INTEGER NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
    ts           INTEGER NOT NULL,            -- unix seconds
    up           INTEGER NOT NULL DEFAULT 0,  -- 1 while the stream was live
    bitrate_kbps REAL NOT NULL DEFAULT 0,
    fps          REAL NOT NULL DEFAULT 0,
    bytes_sent   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_metric_samples ON metric_samples(stream_id, ts);
