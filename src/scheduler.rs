//! Background scheduler that starts/stops streams at their configured times.

use std::time::Duration;

use sqlx::SqlitePool;

use crate::supervisor::StreamManager;

/// Spawn the scheduler loop (ticks every 30s).
pub fn spawn(db: SqlitePool, manager: StreamManager) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if let Err(e) = tick(&db, &manager).await {
                tracing::warn!("scheduler tick failed: {e}");
            }
        }
    });
}

async fn tick(db: &SqlitePool, manager: &StreamManager) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();

    // Start streams whose scheduled_start just passed (within a 2.5 min window so a
    // restart of the app doesn't fire long-past schedules).
    let to_start: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM streams \
         WHERE scheduled_start IS NOT NULL \
           AND scheduled_start <= ? AND scheduled_start > ? \
           AND status != 'live'",
    )
    .bind(now)
    .bind(now - 150)
    .fetch_all(db)
    .await?;
    for id in to_start {
        if manager.is_running(id).await {
            continue;
        }
        tracing::info!("scheduler: starting stream {id}");
        if let Err(e) = manager.start(id).await {
            tracing::warn!("scheduler: could not start stream {id}: {e}");
            let _ = sqlx::query("UPDATE streams SET last_error = ? WHERE id = ?")
                .bind(format!("scheduled start failed: {e}"))
                .bind(id)
                .execute(db)
                .await;
        }
    }

    // Stop streams whose scheduled_stop has passed.
    let to_stop: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM streams \
         WHERE scheduled_stop IS NOT NULL \
           AND scheduled_stop <= ? AND status = 'live'",
    )
    .bind(now)
    .fetch_all(db)
    .await?;
    for id in to_stop {
        tracing::info!("scheduler: stopping stream {id}");
        let _ = manager.stop(id).await;
    }

    Ok(())
}
