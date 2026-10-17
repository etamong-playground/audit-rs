#![cfg(feature = "postgres")]

use chrono::{Duration, Utc};
use etamong_audit::durable::{
    new_id,
    postgres::{record, SCHEMA},
    Event,
};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, PgPool};

async fn database() -> (PgPool, PgPool, String) {
    let url = std::env::var("AUDIT_TEST_DATABASE_URL")
        .expect("postgres feature tests require AUDIT_TEST_DATABASE_URL");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("audit_rs_test_{}", new_id());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(move |conn, _| {
            let query = format!("SET search_path TO {path}");
            Box::pin(async move {
                sqlx::query(&query).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
    sqlx::query("CREATE TABLE business (id integer PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();
    (admin, pool, schema)
}
async fn cleanup(admin: PgPool, pool: PgPool, schema: String) {
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
fn event() -> Event {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/go-durable.json")).unwrap();
    let mut e: Event = serde_json::from_value(fixture["events"][0]["event"].clone()).unwrap();
    e.event_id = new_id();
    e.operation_id = new_id();
    e.occurred_at = Utc::now().fixed_offset();
    e
}
async fn counts(pool: &PgPool) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM business),(SELECT count(*) FROM audit_events),(SELECT count(*) FROM audit_delivery)").fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn mutation_event_and_delivery_share_commit_or_rollback() {
    let (admin, pool, schema) = database().await;
    for commit in [false, true] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO business VALUES (1)")
            .execute(&mut *tx)
            .await
            .unwrap();
        let e = event();
        record(&mut tx, &e).await.unwrap();
        assert_eq!(
            counts(&pool).await,
            (0, 0, 0),
            "record committed caller transaction"
        );
        if commit {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
        let n = if commit { 1 } else { 0 };
        assert_eq!(counts(&pool).await, (n, n, n));
        if commit {
            let (payload, expires): (String, chrono::DateTime<Utc>) =
                sqlx::query_as("SELECT payload::text,expires_at FROM audit_events")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            let restored = serde_json::from_str::<Event>(&payload).unwrap();
            assert_eq!(restored, e);
            assert_eq!(restored.to_json().unwrap(), e.to_json().unwrap());
            let hash: String = sqlx::query_scalar("SELECT content_hash FROM audit_events")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                hash,
                hex::encode(Sha256::digest(restored.to_json().unwrap()))
            );
            assert_eq!(
                expires.timestamp_micros(),
                e.expires_at().unwrap().timestamp_micros()
            );
        }
    }
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn outbox_failure_aborts_business_and_canonical_insert() {
    let (admin, pool, schema) = database().await;
    sqlx::query("ALTER TABLE audit_delivery ADD CONSTRAINT reject_delivery CHECK (false)")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO business VALUES(1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(record(&mut tx, &event()).await.is_err());
    tx.rollback().await.unwrap();
    assert_eq!(counts(&pool).await, (0, 0, 0));
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn duplicate_id_and_retained_mutations_are_rejected() {
    let (admin, pool, schema) = database().await;
    let e = event();
    let mut tx = pool.begin().await.unwrap();
    record(&mut tx, &e).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO business VALUES(1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(record(&mut tx, &e).await.is_err());
    tx.rollback().await.unwrap();
    for query in [
        "UPDATE audit_events SET content_hash='forged'",
        "DELETE FROM audit_events",
        "TRUNCATE audit_events CASCADE",
    ] {
        assert!(
            sqlx::query(query).execute(&pool).await.is_err(),
            "accepted {query}"
        );
    }
    assert_eq!(counts(&pool).await, (0, 1, 1));
    sqlx::query("UPDATE audit_delivery SET attempts=1")
        .execute(&pool)
        .await
        .unwrap();
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn invalid_or_out_of_window_event_does_not_persist() {
    let (admin, pool, schema) = database().await;
    for offset in [Duration::days(-800), Duration::minutes(6)] {
        let mut e = event();
        e.occurred_at = (Utc::now() + offset).fixed_offset();
        let mut tx = pool.begin().await.unwrap();
        assert!(record(&mut tx, &e).await.is_err());
        tx.rollback().await.unwrap();
    }
    let mut e = event();
    e.actor.key_id.clear();
    let mut tx = pool.begin().await.unwrap();
    assert!(record(&mut tx, &e).await.is_err());
    tx.rollback().await.unwrap();
    assert_eq!(counts(&pool).await, (0, 0, 0));
    cleanup(admin, pool, schema).await;
}
