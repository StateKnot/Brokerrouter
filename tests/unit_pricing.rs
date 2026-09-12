use brokerrouter::{
    accounting::{self, Admission},
    error::Error,
    money::{self, UnitPriceSnapshot, UnitRate, UnitUsage},
    store,
};
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

fn quote() -> UnitPriceSnapshot {
    UnitPriceSnapshot {
        schema: "units-v1".into(),
        price_version: Uuid::new_v4(),
        currency: "CNY".into(),
        cny_per_unit: Decimal::ONE,
        fx_version: None,
        rates: vec![
            UnitRate {
                unit: "image_1024".into(),
                price_per_unit: money::nonnegative("0.08").unwrap(),
                maximum_quantity: Decimal::from(4),
            },
            UnitRate {
                unit: "video_second_720p".into(),
                price_per_unit: money::nonnegative("0.3").unwrap(),
                maximum_quantity: Decimal::from(10),
            },
        ],
    }
}
fn usage(images: &str, seconds: &str) -> Vec<UnitUsage> {
    vec![
        UnitUsage {
            unit: "image_1024".into(),
            quantity: money::nonnegative(images).unwrap(),
        },
        UnitUsage {
            unit: "video_second_720p".into(),
            quantity: money::nonnegative(seconds).unwrap(),
        },
    ]
}
#[test]
fn non_token_quotes_are_exact_bounded_and_require_every_usage_dimension() {
    let mut q = quote();
    assert_eq!(q.maximum_cost().unwrap(), 3_320_000);
    let cost = q.breakdown(&usage("2", "1.25")).unwrap();
    assert_eq!(cost.cny_micros, 535_000);
    assert_eq!(cost.original_currency, "CNY");
    q.currency = "USD".into();
    q.cny_per_unit = money::nonnegative("7.123456789").unwrap();
    q.fx_version = Some(1);
    assert_eq!(
        q.breakdown(&usage("2", "1.25")).unwrap().cny_micros,
        3_811_050
    );
    assert!(q.breakdown(&usage("5", "0")).is_err());
    assert!(q.breakdown(&usage("0", "10.000001")).is_err());
    assert!(q.breakdown(&[]).is_err());
    let mut duplicate = usage("1", "1");
    duplicate[1] = duplicate[0].clone();
    assert!(q.breakdown(&duplicate).is_err());
    q.rates[1].unit = q.rates[0].unit.clone();
    assert!(q.validate().is_err());
    let mut q = quote();
    q.rates[0].price_per_unit = money::nonnegative("0.000000000001").unwrap();
    assert_eq!(q.breakdown(&usage("1", "0")).unwrap().cny_micros, 1);
    q.schema = "unknown".into();
    assert!(q.maximum_cost().is_err());
}

#[test]
fn unrepresentable_exact_quote_is_rejected_instead_of_silently_rounded() {
    let mut q = quote();
    q.rates[0].price_per_unit = money::nonnegative("123456789.123456789123").unwrap();
    q.rates[0].maximum_quantity = money::nonnegative("123456789.123456789123").unwrap();
    assert!(q.maximum_cost().is_err());
    q.rates[0].price_per_unit = money::nonnegative("0.000000000001").unwrap();
    q.rates[0].maximum_quantity = Decimal::ONE;
    assert_eq!(
        q.breakdown(&usage("0.000000000001", "0"))
            .unwrap()
            .cny_micros,
        1
    );
    q.currency = "USD".into();
    q.cny_per_unit = money::nonnegative("0.0000000001").unwrap();
    q.fx_version = Some(1);
    assert!(q.breakdown(&usage("0.000000000001", "0")).is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn unit_billing_freezes_fx_and_settles_once_even_after_revocation(pool: PgPool) {
    let tenant = store::create_tenant(&pool, "media", 100_000_000)
        .await
        .unwrap();
    store::put_model(&pool, tenant, "media/model", "Media")
        .await
        .unwrap();
    let (key_id, _) = store::create_key(
        &pool,
        tenant,
        "media",
        100_000_000,
        &["media/model".into()],
        None,
    )
    .await
    .unwrap();
    let who = store::Identity {
        tenant_id: tenant,
        key_id,
    };
    let fx = store::set_fx(
        &pool,
        tenant,
        money::nonnegative("7.123456789").unwrap(),
        Utc::now() - Duration::seconds(2),
        "test",
    )
    .await
    .unwrap();
    let mut q = quote();
    q.currency = "USD".into();
    q.cny_per_unit = money::nonnegative("7.123456789").unwrap();
    q.fx_version = Some(fx);
    let maximum = q.maximum_cost().unwrap();
    let hash = store::hash(b"non-token operation");
    let admission = |q: UnitPriceSnapshot, maximum_micros| Admission {
        identity: &who,
        model: "media/model",
        idempotency_key: "units",
        request_hash: &hash,
        maximum_micros,
        snapshot: q,
        routing: None,
        timeout_seconds: 60,
        governance_turn_id: None,
    };
    assert!(
        accounting::reserve_units(&pool, admission(q.clone(), maximum - 1))
            .await
            .is_err()
    );
    let (reserved, new) = accounting::reserve_units(&pool, admission(q.clone(), maximum))
        .await
        .unwrap();
    assert!(new);
    assert!(
        !accounting::reserve_units(&pool, admission(q, maximum))
            .await
            .unwrap()
            .1
    );
    store::set_fx(
        &pool,
        tenant,
        Decimal::from(8),
        Utc::now() - Duration::seconds(1),
        "changed after admission",
    )
    .await
    .unwrap();
    accounting::mark_submitting(&pool, &who, reserved.id)
        .await
        .unwrap();
    accounting::mark_unknown(&pool, &who, reserved.id, "connection_lost")
        .await
        .unwrap();
    assert!(
        accounting::release_unsubmitted(&pool, &who, reserved.id)
            .await
            .is_err()
    );
    assert!(
        accounting::settle_units(&pool, &who, reserved.id, &usage("0", "11"))
            .await
            .is_err()
    );
    assert!(
        accounting::settle_usage(&pool, &who, reserved.id, 1, 1, None)
            .await
            .is_err()
    );
    store::revoke_key(&pool, tenant, key_id).await.unwrap();
    accounting::settle_units(&pool, &who, reserved.id, &usage("2", "1.25"))
        .await
        .unwrap();
    let mut reordered = usage("2", "1.25");
    reordered.reverse();
    accounting::settle_units(&pool, &who, reserved.id, &reordered)
        .await
        .unwrap();
    assert!(matches!(
        accounting::settle_units(&pool, &who, reserved.id, &usage("1", "1.25")).await,
        Err(Error::Conflict)
    ));
    let balance: (i64, i64) =
        sqlx::query_as("SELECT spent_micros,reserved_micros FROM tenants WHERE id=$1")
            .bind(tenant)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(balance, (3_811_050, 0));
    let details: serde_json::Value = sqlx::query_scalar(
        "SELECT details FROM ledger_entries WHERE request_id=$1 AND kind='finalize'",
    )
    .bind(reserved.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(details["cost"]["original_currency"], "USD");
    assert_eq!(details["usage_units"][1]["quantity"], "1.25");
    assert!(details.get("input_tokens").is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ledger_entries WHERE request_id=$1")
            .bind(reserved.id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        3
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn unit_reservation_cannot_borrow_another_tenants_fx(pool: PgPool) {
    let a = store::create_tenant(&pool, "a", 100_000_000).await.unwrap();
    let b = store::create_tenant(&pool, "b", 100_000_000).await.unwrap();
    store::put_model(&pool, b, "media/model", "Media")
        .await
        .unwrap();
    let (key_id, _) = store::create_key(&pool, b, "b", 100_000_000, &["media/model".into()], None)
        .await
        .unwrap();
    let who = store::Identity {
        tenant_id: b,
        key_id,
    };
    let fx = store::set_fx(
        &pool,
        a,
        Decimal::from(7),
        Utc::now() - Duration::seconds(1),
        "a only",
    )
    .await
    .unwrap();
    let mut q = quote();
    q.currency = "USD".into();
    q.cny_per_unit = Decimal::from(7);
    q.fx_version = Some(fx);
    assert!(
        accounting::reserve_units(
            &pool,
            Admission {
                identity: &who,
                model: "media/model",
                idempotency_key: "foreign-fx",
                request_hash: &store::hash(b"foreign"),
                maximum_micros: q.maximum_cost().unwrap(),
                snapshot: q,
                routing: None,
                timeout_seconds: 60,
                governance_turn_id: None
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT reserved_micros FROM tenants WHERE id=$1")
            .bind(b)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
