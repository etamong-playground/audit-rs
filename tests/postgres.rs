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

async fn save(pool: &PgPool, e: &Event) {
    let mut tx = pool.begin().await.unwrap();
    record(&mut tx, e).await.unwrap();
    tx.commit().await.unwrap();
}
async fn retry_now(pool: &PgPool) {
    sqlx::query("UPDATE audit_delivery SET next_attempt_at=now()")
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn export_replays_identical_event_after_lost_acknowledgement() {
    use etamong_audit::durable::postgres::{export_one, Disposition};
    let (admin, pool, schema) = database().await;
    let e = event();
    save(&pool, &e).await;
    let central = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    for fail in [true, false] {
        let central = central.clone();
        let expected = e.clone();
        let result = export_one(&pool, move |received| async move {
            assert_eq!(received, expected);
            let payload = received.to_json().unwrap();
            let mut events = central.lock().unwrap();
            if let Some(old) = events.insert(received.event_id, payload.clone()) {
                assert_eq!(old, payload);
            }
            if fail {
                Err(())
            } else {
                Ok(Disposition::Accepted)
            }
        })
        .await;
        if fail {
            assert!(result.is_err());
            let (attempts,ack,delayed):(i32,bool,bool)=sqlx::query_as("SELECT attempts,acknowledged_at IS NOT NULL,next_attempt_at>now() FROM audit_delivery").fetch_one(&pool).await.unwrap();
            assert_eq!((attempts, ack, delayed), (1, false, true));
            assert!(!export_one(&pool, |_| async {
                panic!("backoff bypassed");
                #[allow(unreachable_code)]
                Ok::<_, ()>(Disposition::Accepted)
            })
            .await
            .unwrap());
            retry_now(&pool).await;
        } else {
            assert!(result.unwrap());
        }
    }
    assert_eq!(central.lock().unwrap().len(), 1);
    assert!(
        !export_one(&pool, |_| async { Ok::<_, ()>(Disposition::Accepted) })
            .await
            .unwrap()
    );
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn premature_expired_ack_is_retried_with_bounded_backoff() {
    use etamong_audit::durable::postgres::{export_one, Disposition};
    let (admin, pool, schema) = database().await;
    save(&pool, &event()).await;
    sqlx::query("UPDATE audit_delivery SET attempts=2147483647")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        export_one(&pool, |_| async { Ok::<_, ()>(Disposition::Expired) })
            .await
            .is_err()
    );
    let (attempts,ack,delay):(i32,bool,f64)=sqlx::query_as("SELECT attempts,acknowledged_at IS NOT NULL,EXTRACT(EPOCH FROM next_attempt_at-now())::double precision FROM audit_delivery").fetch_one(&pool).await.unwrap();
    assert_eq!((attempts, ack), (2147483647, false));
    assert!((510.0..=513.0).contains(&delay));
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn concurrent_exporters_skip_locked_event() {
    use etamong_audit::durable::postgres::{export_one, Disposition};
    let (admin, pool, schema) = database().await;
    save(&pool, &event()).await;
    let (started, waiting) = tokio::sync::oneshot::channel();
    let (release, blocked) = tokio::sync::oneshot::channel();
    let worker_pool = pool.clone();
    let worker = tokio::spawn(async move {
        export_one(&worker_pool, |_| async move {
            started.send(()).unwrap();
            blocked.await.unwrap();
            Ok::<_, ()>(Disposition::Accepted)
        })
        .await
    });
    waiting.await.unwrap();
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        export_one(&pool, |_| async {
            panic!("locked event dispatched twice");
            #[allow(unreachable_code)]
            Ok::<_, ()>(Disposition::Accepted)
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!second);
    release.send(()).unwrap();
    assert!(worker.await.unwrap().unwrap());
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn failed_ack_persistence_leaves_event_for_replay() {
    use etamong_audit::durable::postgres::{export_one, Disposition};
    let (admin, pool, schema) = database().await;
    save(&pool, &event()).await;
    sqlx::query(
        "ALTER TABLE audit_delivery ADD CONSTRAINT reject_ack CHECK(acknowledged_at IS NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        export_one(&pool, |_| async { Ok::<_, ()>(Disposition::Accepted) })
            .await
            .is_err()
    );
    let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM audit_delivery")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
    sqlx::query("ALTER TABLE audit_delivery DROP CONSTRAINT reject_ack")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        export_one(&pool, |_| async { Ok::<_, ()>(Disposition::Accepted) })
            .await
            .unwrap()
    );
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn stalled_sender_is_cancelled_and_retry_is_persisted() {
    use etamong_audit::durable::postgres::{export_one, Disposition};
    let (admin, pool, schema) = database().await;
    save(&pool, &event()).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        export_one(&pool, |_| std::future::pending::<Result<Disposition, ()>>()),
    )
    .await
    .expect("sender timeout did not release transaction");
    assert!(result.is_err());
    let (attempts, ack): (i32, bool) =
        sqlx::query_as("SELECT attempts,acknowledged_at IS NOT NULL FROM audit_delivery")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((attempts, ack), (1, false));
    retry_now(&pool).await;
    assert!(
        export_one(&pool, |_| async { Ok::<_, ()>(Disposition::Accepted) })
            .await
            .unwrap()
    );
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn corrupted_retained_hash_never_reaches_sender() {
    use etamong_audit::durable::postgres::{export_one, Disposition, ExportError};
    let (admin, pool, schema) = database().await;
    save(&pool, &event()).await;
    sqlx::query("ALTER TABLE audit_events DISABLE TRIGGER audit_event_immutable")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE audit_events SET content_hash='corrupt'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE audit_events ENABLE TRIGGER audit_event_immutable")
        .execute(&pool)
        .await
        .unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sent = calls.clone();
    let result = export_one(&pool, move |_| async move {
        sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok::<_, ()>(Disposition::Accepted)
    })
    .await;
    assert!(matches!(result, Err(ExportError::InvalidStoredEvent)));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    save(&pool, &event()).await;
    assert!(
        export_one(&pool, |_| async { Ok::<_, ()>(Disposition::Accepted) })
            .await
            .unwrap(),
        "corrupt oldest event blocked later delivery"
    );
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn slow_acquisition_reserves_time_for_retry_commit() {
    use etamong_audit::durable::postgres::{export_one, Disposition};
    let (admin, pool, schema) = database().await;
    save(&pool, &event()).await;
    let first = pool.acquire().await.unwrap();
    let second = pool.acquire().await.unwrap();
    let third = pool.acquire().await.unwrap();
    let worker_pool = pool.clone();
    let worker = tokio::spawn(async move {
        export_one(&worker_pool, |_| {
            std::future::pending::<Result<Disposition, ()>>()
        })
        .await
    });
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    drop(first);
    drop(second);
    drop(third);
    assert!(worker.await.unwrap().is_err());
    let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM audit_delivery")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1, "pass cancelled before retry commit");
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn postgres_rounded_timestamp_preserves_canonical_event() {
    use chrono::Timelike;
    use etamong_audit::durable::postgres::{export_one, Disposition};
    let (admin, pool, schema) = database().await;
    let mut e = event();
    e.occurred_at = e.occurred_at.with_nanosecond(123456789).unwrap();
    let payload = String::from_utf8(e.to_json().unwrap()).unwrap();
    let hash = hex::encode(Sha256::digest(payload.as_bytes()));
    sqlx::query("INSERT INTO audit_events(service,event_id,payload,content_hash,occurred_at,expires_at) VALUES($1,$2,$3::jsonb,$4,$5::text::timestamptz,$6::text::timestamptz)")
        .bind(&e.service).bind(&e.event_id).bind(payload).bind(hash).bind(e.occurred_at.to_rfc3339()).bind(e.expires_at().unwrap().to_rfc3339()).execute(&pool).await.unwrap();
    assert!(export_one(&pool, |sent| async move {
        assert_eq!(sent, e);
        Ok::<_, ()>(Disposition::Accepted)
    })
    .await
    .unwrap());
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn acquisition_stops_before_persistence_reserve() {
    use etamong_audit::durable::postgres::{export_one, Disposition, ExportError};
    let (admin, pool, schema) = database().await;
    save(&pool, &event()).await;
    let first = pool.acquire().await.unwrap();
    let second = pool.acquire().await.unwrap();
    let third = pool.acquire().await.unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sent = calls.clone();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        export_one(&pool, move |_| async move {
            sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, ()>(Disposition::Accepted)
        }),
    )
    .await
    .expect("acquisition consumed persistence reserve");
    assert!(matches!(outcome, Err(ExportError::Timeout)));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    drop(first);
    drop(second);
    drop(third);
    assert!(
        export_one(&pool, |_| async { Ok::<_, ()>(Disposition::Accepted) })
            .await
            .unwrap()
    );
    cleanup(admin, pool, schema).await;
}

async fn retained_fixture(pool: &PgPool, e: &Event) {
    let payload = String::from_utf8(e.to_json().unwrap()).unwrap();
    let hash = hex::encode(Sha256::digest(payload.as_bytes()));
    sqlx::query("INSERT INTO audit_events(service,event_id,payload,content_hash,occurred_at,expires_at) VALUES($1,$2,$3::jsonb,$4,$5,$6)")
        .bind(&e.service).bind(&e.event_id).bind(payload).bind(hash).bind(e.occurred_at).bind(e.expires_at().unwrap()).execute(pool).await.unwrap();
}

#[tokio::test]
async fn cleanup_is_bounded_and_rolls_back_with_its_summary() {
    use etamong_audit::durable::postgres::purge_expired;
    let (admin, pool, schema) = database().await;
    let mut first = event();
    first.occurred_at = "2020-02-29T12:00:00Z".parse().unwrap();
    retained_fixture(&pool, &first).await;
    let mut second = event();
    second.occurred_at = "2021-06-15T12:00:00Z".parse().unwrap();
    retained_fixture(&pool, &second).await;
    let mut live = event();
    live.event_id = first.event_id.clone();
    live.service = "another-service".into();
    save(&pool, &live).await;
    for commit in [false, true] {
        let mut tx = pool.begin().await.unwrap();
        let removed = purge_expired(&mut tx, 1).await.unwrap();
        assert_eq!(removed.removed, 1);
        assert_eq!(
            removed.occurred_from,
            Some(first.occurred_at.with_timezone(&Utc))
        );
        assert_eq!(removed.occurred_to, removed.occurred_from);
        assert_eq!(removed.expired_from, Some(first.expires_at().unwrap()));
        assert_eq!(removed.expired_to, removed.expired_from);
        let mut summary = event();
        summary.action = "audit.retention.cleanup".into();
        summary.changes = vec![etamong_audit::durable::Change {
            field: "removed_count".into(),
            before: String::new(),
            after: removed.removed.to_string(),
        }];
        record(&mut tx, &summary).await.unwrap();
        assert_eq!(
            counts(&pool).await,
            (0, 3, 3),
            "cleanup escaped caller transaction"
        );
        if commit {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
        assert_eq!(counts(&pool).await, (0, 3, 3));
    }
    let retained: Vec<String> =
        sqlx::query_scalar("SELECT event_id FROM audit_events WHERE service=$1")
            .bind(&live.service)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(retained, vec![live.event_id]);
    let mut third = event();
    third.occurred_at = "2022-06-15T12:00:00Z".parse().unwrap();
    retained_fixture(&pool, &third).await;
    let mut tx = pool.begin().await.unwrap();
    let remaining = purge_expired(&mut tx, 100).await.unwrap();
    assert_eq!(remaining.removed, 2);
    assert_eq!(
        remaining.occurred_from,
        Some(second.occurred_at.with_timezone(&Utc))
    );
    assert_eq!(
        remaining.occurred_to,
        Some(third.occurred_at.with_timezone(&Utc))
    );
    assert_eq!(remaining.expired_from, Some(second.expires_at().unwrap()));
    assert_eq!(remaining.expired_to, Some(third.expires_at().unwrap()));
    tx.commit().await.unwrap();
    assert_eq!(counts(&pool).await, (0, 2, 2));
    let mut tx = pool.begin().await.unwrap();
    let empty = purge_expired(&mut tx, 100).await.unwrap();
    assert_eq!(empty.removed, 0);
    assert_eq!(
        (
            empty.occurred_from,
            empty.occurred_to,
            empty.expired_from,
            empty.expired_to
        ),
        (None, None, None, None)
    );
    tx.rollback().await.unwrap();
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn cleanup_summary_insert_failure_restores_expired_event() {
    use etamong_audit::durable::postgres::purge_expired;
    let (admin, pool, schema) = database().await;
    let mut expired = event();
    expired.occurred_at = "2020-01-01T00:00:00Z".parse().unwrap();
    retained_fixture(&pool, &expired).await;
    sqlx::query("ALTER TABLE audit_delivery ADD CONSTRAINT reject_summary CHECK(event_id <> 'cleanup-summary')").execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(purge_expired(&mut tx, 10).await.unwrap().removed, 1);
    let mut summary = event();
    summary.event_id = "cleanup-summary".into();
    summary.action = "audit.retention.cleanup".into();
    assert!(record(&mut tx, &summary).await.is_err());
    tx.rollback().await.unwrap();
    assert_eq!(counts(&pool).await, (0, 1, 1));
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn cleanup_rejects_invalid_limits_without_deleting() {
    use etamong_audit::durable::postgres::{purge_expired, CleanupError};
    let (admin, pool, schema) = database().await;
    let mut expired = event();
    expired.occurred_at = "2020-01-01T00:00:00Z".parse().unwrap();
    retained_fixture(&pool, &expired).await;
    let mut tx = pool.begin().await.unwrap();
    for limit in [0, 10001, u32::MAX] {
        assert!(matches!(
            purge_expired(&mut tx, limit).await,
            Err(CleanupError::InvalidLimit)
        ));
    }
    tx.rollback().await.unwrap();
    assert_eq!(counts(&pool).await, (0, 1, 1));
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
async fn enqueue_uses_event_schema_after_install_and_upgrade() {
    for upgrade in [false, true] {
        let (admin, pool, schema) = database().await;
        if upgrade {
            sqlx::raw_sql("CREATE OR REPLACE FUNCTION audit_enqueue_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit_delivery (service,event_id) VALUES (NEW.service,NEW.event_id); RETURN NEW; END; $$;")
                .execute(&pool).await.unwrap();
        }
        let retained = event();
        let mut tx = pool.begin().await.unwrap();
        record(&mut tx, &retained).await.unwrap();
        tx.commit().await.unwrap();
        sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
        sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "CREATE TEMP TABLE audit_delivery (LIKE audit_delivery INCLUDING ALL) ON COMMIT DROP",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO business VALUES (1)")
            .execute(&mut *tx)
            .await
            .unwrap();
        let added = event();
        record(&mut tx, &added).await.unwrap();
        let shadow: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_temp.audit_delivery")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(shadow, 0, "temporary table received durable delivery");
        tx.commit().await.unwrap();
        assert_eq!(counts(&pool).await, (1, 2, 2));
        for expected in [retained, added] {
            let payload: String = sqlx::query_scalar("SELECT e.payload::text FROM audit_events e JOIN audit_delivery d USING(service,event_id) WHERE e.event_id=$1 AND e.service=$2")
                .bind(&expected.event_id).bind(&expected.service).fetch_one(&pool).await.unwrap();
            assert_eq!(serde_json::from_str::<Event>(&payload).unwrap(), expected);
        }
        cleanup(admin, pool, schema).await;
    }
}
