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
pub use judge::McpJudgeWorker;
pub mod mcp;
pub mod mcp_calls;
pub mod mcp_catalog;
pub mod mcp_client;
pub mod mcp_oauth;
pub mod mcp_server;
pub mod mcp_skill_approvals;
pub mod mcp_skill_resources;
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
pub mod runtime_artifact_api;
pub mod runtime_artifacts;
pub mod runtime_publications;
pub mod runtime_results;
pub mod scanner_engine;
pub mod scanner_worker;
pub mod settings;
pub mod settings_api;
pub mod store;
mod tls_roots;
pub mod traces;
pub mod transport;

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let maximum = database_max_connections(std::env::var("BROKERROUTER_DATABASE_MAX_CONNECTIONS"))?;
    PgPoolOptions::new()
        .max_connections(maximum)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|connection, _| Box::pin(configure_connection(connection)))
        .connect(database_url)
        .await
}

fn database_max_connections(value: Result<String, std::env::VarError>) -> Result<u32, sqlx::Error> {
    match value {
        Err(std::env::VarError::NotPresent) => Ok(20),
        Ok(value) => value
            .parse::<u32>()
            .ok()
            .filter(|maximum| (1..=64).contains(maximum))
            .ok_or_else(database_pool_configuration_error),
        Err(std::env::VarError::NotUnicode(_)) => Err(database_pool_configuration_error()),
    }
}

fn database_pool_configuration_error() -> sqlx::Error {
    sqlx::Error::Configuration(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "BROKERROUTER_DATABASE_MAX_CONNECTIONS must be 1..64",
    )))
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
    #[test]
    fn database_pool_budget_is_bounded_and_defaults_remain_compatible() {
        assert_eq!(
            database_max_connections(Err(std::env::VarError::NotPresent)).unwrap(),
            20
        );
        for value in ["1", "2", "8", "20", "64"] {
            assert_eq!(
                database_max_connections(Ok(value.into())).unwrap(),
                value.parse::<u32>().unwrap()
            );
        }
        for value in ["", "0", "65", "-1", "two", "2 ", "4294967296"] {
            assert!(database_max_connections(Ok(value.into())).is_err());
        }
        assert!(
            database_max_connections(Err(std::env::VarError::NotUnicode(
                std::ffi::OsString::from("invalid")
            )))
            .is_err()
        );
    }
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
