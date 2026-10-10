mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use brokerrouter::governance_api::{Control, control_router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

async fn probe(pool: &PgPool, path: &str) -> (StatusCode, Vec<u8>) {
    let app = control_router(Control {
        pool: pool.clone(),
        verifier: Arc::new(common::verifier()),
        cipher: Arc::new(common::cipher()),
    });
    let response = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    (
        response.status(),
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

async fn control_pool(pool: &PgPool) -> PgPool {
    sqlx::raw_sql(include_str!("../deploy/permissions.sql"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_governance_control")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn control_readiness_uses_the_actual_purpose_role_and_rejects_schema_drift(pool: PgPool) {
    let control = control_pool(&pool).await;
    assert_eq!(
        probe(&control, "/health/ready").await,
        (StatusCode::NO_CONTENT, vec![])
    );
    sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(99999,'future fixture',true,decode('11','hex'),0)").execute(&pool).await.unwrap();
    let (status, body) = probe(&control, "/health/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["code"],
        "schema_mismatch"
    );
    assert_eq!(
        probe(&control, "/health/live").await,
        (StatusCode::NO_CONTENT, vec![])
    );
    control.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn control_readiness_is_bounded_when_its_database_pool_is_exhausted(pool: PgPool) {
    let control = control_pool(&pool).await;
    // Build the fixture verifier before measuring the HTTP/DB readiness budget.
    assert_eq!(
        probe(&control, "/health/live").await.0,
        StatusCode::NO_CONTENT
    );
    let held = control.acquire().await.unwrap();
    let (status, body) =
        tokio::time::timeout(Duration::from_secs(5), probe(&control, "/health/ready"))
            .await
            .unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["code"],
        "persistence_unavailable"
    );
    assert_eq!(
        probe(&control, "/health/live").await,
        (StatusCode::NO_CONTENT, vec![])
    );
    drop(held);
    assert_eq!(
        probe(&control, "/health/ready").await,
        (StatusCode::NO_CONTENT, vec![])
    );
    control.close().await;
    assert_eq!(
        probe(&control, "/health/ready").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        probe(&control, "/health/live").await.0,
        StatusCode::NO_CONTENT
    );
}
