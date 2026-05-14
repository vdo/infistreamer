use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;

/// Open (creating if needed) the SQLite database and run embedded migrations.
pub async fn init(db_path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Create the first admin account when the users table is empty.
pub async fn ensure_admin(pool: &SqlitePool, username: &str, password: &str) -> Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    if count == 0 {
        let hash = crate::auth::hash_password(password)?;
        sqlx::query("INSERT INTO users (username, password_hash, created_at) VALUES (?,?,?)")
            .bind(username)
            .bind(hash)
            .bind(chrono::Utc::now().timestamp())
            .execute(pool)
            .await?;
        tracing::warn!(
            "created initial admin user '{username}' \u{2014} log in and change the password"
        );
    }
    Ok(())
}
