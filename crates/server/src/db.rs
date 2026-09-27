//! Database connection and migrations.
//!
//! One SQLite file holds everything. We open a small connection *pool* so
//! concurrent requests don't serialize on a single connection.

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;

/// Alias so the rest of the server says `Db` instead of `SqlitePool`.
pub type Db = SqlitePool;

/// Open the database at `url` (creating the file if needed) and apply all
/// pending migrations. Returns a ready-to-use pool.
///
/// `url` is an sqlx connection string, e.g. `sqlite://perkele.db` for a file
/// or `sqlite::memory:` for an ephemeral in-memory database.
pub async fn connect(url: &str) -> Result<Db, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(url)?
        // Create the .db file on first run instead of erroring.
        .create_if_missing(true)
        // SQLite ignores foreign-key constraints unless you turn them on, and
        // it's per-connection — so we set it here for every pooled connection.
        .foreign_keys(true)
        // Write-Ahead Logging lets readers and a writer work concurrently
        // instead of locking each other out. Big win for a web server.
        .journal_mode(SqliteJournalMode::Wal)
        // If another connection holds the write lock, wait up to 5s instead of
        // failing immediately with "database is locked".
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;

    run_migrations(&pool).await?;
    Ok(pool)
}

/// Apply embedded migrations. `migrate!` reads `crates/server/migrations/` at
/// compile time and bakes the SQL into the binary, so the deployed server
/// needs no migration files on disk.
pub async fn run_migrations(pool: &Db) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// A fresh, isolated in-memory database for one test.
///
/// Note the `max_connections(1)`: an in-memory SQLite database lives inside a
/// single connection, so a multi-connection pool would hand different callers
/// *different* empty databases. Pinning to one connection keeps all queries
/// pointed at the same in-memory db. Shared by tests across the crate.
#[cfg(test)]
pub(crate) async fn test_pool() -> Db {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory db");
    run_migrations(&pool).await.expect("run migrations");
    pool
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_create_empty_tables() {
        let pool = test_pool().await;

        // Using the runtime-checked query API here (not the compile-time
        // `query!` macro) so this test needs no prepared metadata.
        let families: i64 = sqlx::query_scalar("SELECT count(*) FROM families")
            .fetch_one(&pool)
            .await
            .unwrap();
        let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(families, 0);
        assert_eq!(users, 0);
    }
}
