pub mod accounting;
pub mod api;
pub mod attempts;
pub mod cipher;
pub mod egress;
pub mod error;
pub mod gateway;
pub mod money;
pub mod protocol;
pub mod store;

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

pub async fn check_schema(pool: &PgPool) -> error::Result<()> {
    let applied: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await?;
    let expected: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .collect();
    if applied.len() != expected.len()
        || applied
            .iter()
            .zip(expected)
            .any(|((version, success, checksum), m)| {
                !success || *version != m.version || checksum.as_slice() != m.checksum.as_ref()
            })
    {
        return Err(error::Error::SchemaMismatch);
    }
    Ok(())
}
