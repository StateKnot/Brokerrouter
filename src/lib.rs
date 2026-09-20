pub mod accounting;
pub mod api;
pub mod approval_workbench;
pub mod approvals_api;
pub mod asset_api;
pub mod assets;
pub mod attempts;
pub mod bff;
pub mod catalog;
pub mod catalog_api;
pub mod cipher;
pub mod console;
pub mod console_api;
pub mod egress;
pub mod error;
pub mod gateway;
pub mod governance;
pub mod governance_api;
pub mod governance_settings;
pub mod human;
mod judge;
pub mod mcp;
pub mod mcp_catalog;
pub mod mcp_client;
pub mod mcp_oauth;
pub mod mcp_server;
pub mod media;
pub mod media_api;
pub mod media_protocol;
pub mod media_review;
pub mod media_review_api;
pub mod money;
pub mod monitoring;
pub mod output_delivery;
pub mod output_delivery_api;
pub mod output_scan_api;
pub mod output_scans;
pub mod outputs;
pub mod protocol;
pub mod scanner_engine;
pub mod scanner_worker;
pub mod settings;
pub mod settings_api;
pub mod store;
pub mod traces;

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|connection, _| Box::pin(configure_connection(connection)))
        .connect(database_url)
        .await
}

async fn configure_connection(connection: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    // Preserve explicit nonzero PostgreSQL connection/role settings; never default to infinite waits.
    sqlx::query("SELECT set_config('statement_timeout',CASE WHEN current_setting('statement_timeout')='0' THEN '15000' ELSE current_setting('statement_timeout') END,false),set_config('lock_timeout',CASE WHEN current_setting('lock_timeout')='0' THEN '3000' ELSE current_setting('lock_timeout') END,false)").execute(connection).await?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    #[sqlx::test(migrations = "./migrations")]
    async fn database_waits_are_bounded_without_overriding_explicit_limits(pool: PgPool) {
        let mut c = pool.acquire().await.unwrap();
        sqlx::raw_sql("SET statement_timeout='0';SET lock_timeout='0'")
            .execute(&mut *c)
            .await
            .unwrap();
        configure_connection(&mut c).await.unwrap();
        let limits: (String, String) = sqlx::query_as(
            "SELECT current_setting('statement_timeout'),current_setting('lock_timeout')",
        )
        .fetch_one(&mut *c)
        .await
        .unwrap();
        assert_eq!(limits, ("15s".into(), "3s".into()));
        sqlx::raw_sql("SET statement_timeout='17s';SET lock_timeout='7s'")
            .execute(&mut *c)
            .await
            .unwrap();
        configure_connection(&mut c).await.unwrap();
        let limits: (String, String) = sqlx::query_as(
            "SELECT current_setting('statement_timeout'),current_setting('lock_timeout')",
        )
        .fetch_one(&mut *c)
        .await
        .unwrap();
        assert_eq!(limits, ("17s".into(), "7s".into()));
    }
}
