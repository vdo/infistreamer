//! Background sampler that records per-stream metrics into `metric_samples`,
//! feeding the 24h / 1-week graphs on the monitoring page.

use std::collections::HashMap;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::supervisor::StreamManager;

/// How often a sample row is written per stream.
const SAMPLE_INTERVAL_SECS: u64 = 60;
/// Samples older than this are pruned (a little over a week, so 1w graphs stay full).
const RETENTION_SECS: i64 = 8 * 24 * 3600;

pub fn spawn(db: SqlitePool, manager: StreamManager) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(SAMPLE_INTERVAL_SECS)).await;
            if let Err(e) = tick(&db, &manager).await {
                tracing::warn!("metrics history tick failed: {e}");
            }
        }
    });
}

async fn tick(db: &SqlitePool, manager: &StreamManager) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let stream_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM streams")
        .fetch_all(db)
        .await?;
    let running: HashMap<i64, _> = manager.all_runtime().await.into_iter().collect();

    // Sample every stream (running -> live values, otherwise zeros) so the graphs are
    // continuous and clearly show when each stream was up.
    for id in stream_ids {
        let (up, bitrate, fps, bytes) = match running.get(&id) {
            Some(r) => (1, r.bitrate_kbps, r.fps, r.bytes_sent as i64),
            None => (0, 0.0, 0.0, 0),
        };
        sqlx::query(
            "INSERT INTO metric_samples (stream_id, ts, up, bitrate_kbps, fps, bytes_sent) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(now)
        .bind(up)
        .bind(bitrate)
        .bind(fps)
        .bind(bytes)
        .execute(db)
        .await?;
    }

    sqlx::query("DELETE FROM metric_samples WHERE ts < ?")
        .bind(now - RETENTION_SECS)
        .execute(db)
        .await?;
    Ok(())
}
