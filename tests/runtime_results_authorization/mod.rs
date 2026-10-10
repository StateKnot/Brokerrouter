//! Text-only NULL-set compatibility still requires current model authority at
//! the Human plaintext boundary. The original operation/source remains intact.
use super::*;
use brokerrouter::approval_workbench;
use sqlx::postgres::PgPoolOptions;

async fn reviewer(pool: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET SESSION AUTHORIZATION brokerrouter_approvals")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap()
}

async fn pending_text_only(pool: &PgPool) -> (store::Identity, Uuid, Uuid, Value, PgPool) {
    let (who, turn) = setup(pool).await;
    let request = claimed(pool, &who, turn, "plain-text-current-model").await;
    let saved = runtime_results::record(pool, &common::cipher(), &who, request)
        .await
        .unwrap();
    let result = result_id(&saved);
    assert!(sqlx::query_scalar::<_,bool>("SELECT artifact_set IS NULL AND artifact_set_hash IS NULL FROM runtime_tool_results WHERE tenant_id=$1 AND id=$2")
        .bind(who.tenant_id).bind(result).fetch_one(pool).await.unwrap());
    common::guardrail(
        pool,
        who.tenant_id,
        "plain-text-review",
        1,
        "tool_output",
        json!({"kind":"always"}),
    )
    .await;
    let pending = runtime_results::release(&gateway(pool), &who, result)
        .await
        .unwrap();
    assert_eq!(pending["state"], "waiting_approval");
    assert!(pending.get("text").is_none());
    sqlx::raw_sql(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/deploy/permissions.sql"
    )))
    .execute(pool)
    .await
    .unwrap();
    let reviewer = reviewer(pool).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT current_user")
            .fetch_one(&reviewer)
            .await
            .unwrap(),
        "brokerrouter_approvals"
    );
    let human = common::proof(pool, "alice", "/review", &json!({})).await;
    let positive = approval_workbench::review(
        &reviewer,
        &common::cipher(),
        &human,
        approval_workbench::Review {
            approval_id: common::approval_id(&pending),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        positive["can_continue"], true,
        "valid text-only Human review must remain supported"
    );
    let body: Value = serde_json::from_str(positive["payload_text"].as_str().unwrap()).unwrap();
    assert_eq!(body["text"], "private tool text");
    assert!(body.get("artifact_set").is_none());
    let error = sqlx::query("SELECT result_ciphertext FROM runtime_tool_results LIMIT 1")
        .fetch_all(&reviewer)
        .await
        .unwrap_err();
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("42501")
    );
    (who, turn, result, pending, reviewer)
}

async fn review_after_model_revocation(
    pool: &PgPool,
    who: &store::Identity,
    result: Uuid,
    pending: &Value,
    reviewer: &PgPool,
) {
    assert!(matches!(
        runtime_results::release(&gateway(pool), who, result).await,
        Err(Error::Unauthorized)
    ));
    let human = common::proof(pool, "alice", "/review", &json!({})).await;
    let reviewed = approval_workbench::review(
        reviewer,
        &common::cipher(),
        &human,
        approval_workbench::Review {
            approval_id: common::approval_id(pending),
        },
    )
    .await;
    match reviewed {
        Err(Error::Unauthorized) => {}
        Ok(metadata) => {
            // A legal non-continuable audit view is allowed. It must not include
            // the former body or assert that approval can still continue.
            assert_eq!(metadata["can_continue"], false);
            assert!(
                metadata["payload_text"].is_null() || metadata["payload_text"] == "",
                "current model revocation returned a protected body: {metadata}"
            );
            assert!(!metadata.to_string().contains("private tool text"));
        }
        Err(other) => panic!("current model denial became an unrelated error: {other:?}"),
    }
    let facts: (i64,i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM runtime_tool_results),(SELECT count(*) FROM guardrail_evaluations),(SELECT count(*) FROM requests),(SELECT count(*) FROM guardrail_grants)")
        .fetch_one(pool).await.unwrap();
    assert_eq!(
        facts,
        (1, 0, 0, 0),
        "Human review must never register paid detection or approval authority"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM approvals WHERE tenant_id=$1 AND state='pending'"
        )
        .bind(who.tenant_id)
        .fetch_one(pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn text_only_human_review_after_key_model_revocation_returns_no_body(pool: PgPool) {
    let (who, _turn, result, pending, reviewer) = pending_text_only(&pool).await;
    let deleted =
        sqlx::query("DELETE FROM key_models WHERE tenant_id=$1 AND key_id=$2 AND model_id='model'")
            .bind(who.tenant_id)
            .bind(who.key_id)
            .execute(&pool)
            .await
            .unwrap();
    assert_eq!(deleted.rows_affected(), 1);
    review_after_model_revocation(&pool, &who, result, &pending, &reviewer).await;
    reviewer.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn text_only_human_review_after_model_disable_returns_no_body(pool: PgPool) {
    let (who, _turn, result, pending, reviewer) = pending_text_only(&pool).await;
    let disabled = sqlx::query("UPDATE models SET active=false WHERE tenant_id=$1 AND id='model'")
        .bind(who.tenant_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(disabled.rows_affected(), 1);
    review_after_model_revocation(&pool, &who, result, &pending, &reviewer).await;
    reviewer.close().await;
}
