use super::{Event, InvalidEvent};
use chrono::{Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use std::{error::Error, fmt};

/// Apply through the application's migration driver, outside business transactions.
/// Database owners can bypass guards; use a restricted runtime database role.
pub const SCHEMA: &str = include_str!("schema.sql");

#[derive(Debug)]
pub enum RecordError {
    Invalid(InvalidEvent),
    Occurrence,
    Database(sqlx::Error),
}
impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(_) => f.write_str("invalid audit event"),
            Self::Occurrence => f.write_str("audit occurrence is future-dated or already expired"),
            Self::Database(_) => f.write_str("audit persistence failed"),
        }
    }
}
impl Error for RecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(err) => Some(err),
            Self::Database(err) => Some(err),
            Self::Occurrence => None,
        }
    }
}

/// Writes the event and its delivery row in the caller's transaction, without committing.
/// Roll back the entire business transaction on any error, including validation failure.
pub async fn record(tx: &mut Transaction<'_, Postgres>, event: &Event) -> Result<(), RecordError> {
    let payload = event.to_json().map_err(RecordError::Invalid)?;
    let expires_at = event.expires_at().map_err(RecordError::Invalid)?;
    let now = Utc::now();
    if event.occurred_at > now + Duration::minutes(5) || expires_at <= now {
        return Err(RecordError::Occurrence);
    }
    let hash = hex::encode(Sha256::digest(&payload));
    let payload = String::from_utf8(payload).expect("serialized JSON is valid UTF-8");
    sqlx::query("INSERT INTO audit_events (service,event_id,payload,content_hash,occurred_at,expires_at) VALUES ($1,$2,$3::jsonb,$4,$5,$6)")
        .bind(&event.service)
        .bind(&event.event_id)
        .bind(payload)
        .bind(hash)
        .bind(event.occurred_at)
        .bind(expires_at)
        .execute(&mut **tx)
        .await
        .map_err(RecordError::Database)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Accepted,
    Expired,
}

#[derive(Debug)]
pub enum ExportError {
    Database(sqlx::Error),
    InvalidStoredEvent,
    Delivery,
    Timeout,
}
impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Database(_) => "audit delivery persistence failed",
            Self::InvalidStoredEvent => "invalid retained audit event",
            Self::Delivery => "audit delivery not acknowledged",
            Self::Timeout => "audit export deadline exceeded",
        })
    }
}
impl Error for ExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            _ => None,
        }
    }
}

/// Locks one pending row, invokes the sender, then persists acknowledgement or backoff.
/// Returns false when no eligible row exists. Sender errors are not retained in history.
/// The sender must authenticate the collector and acknowledge only after central commit.
/// Its future is dropped after ten seconds; remote effects may already have happened, so
/// retries must use event identity for deduplication. The entire pass is bounded to 15 seconds.
pub async fn export_one<F, Fut, E>(pool: &sqlx::PgPool, send: F) -> Result<bool, ExportError>
where
    F: FnOnce(Event) -> Fut,
    Fut: std::future::Future<Output = Result<Disposition, E>>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    tokio::time::timeout_at(deadline, export_pending(pool, send, deadline))
        .await
        .map_err(|_| ExportError::Timeout)?
}

type PendingDelivery = (
    String,
    String,
    String,
    String,
    chrono::DateTime<Utc>,
    chrono::DateTime<Utc>,
    i32,
);

async fn export_pending<F, Fut, E>(
    pool: &sqlx::PgPool,
    send: F,
    deadline: tokio::time::Instant,
) -> Result<bool, ExportError>
where
    F: FnOnce(Event) -> Fut,
    Fut: std::future::Future<Output = Result<Disposition, E>>,
{
    let work_deadline = deadline - std::time::Duration::from_secs(5);
    let mut tx = tokio::time::timeout_at(work_deadline, pool.begin())
        .await
        .map_err(|_| ExportError::Timeout)?
        .map_err(ExportError::Database)?;
    let row: Option<PendingDelivery> = tokio::time::timeout_at(work_deadline, sqlx::query_as(
        "SELECT e.service,e.event_id,e.payload::text,e.content_hash,e.occurred_at,e.expires_at,d.attempts
         FROM audit_delivery d JOIN audit_events e USING(service,event_id)
         WHERE d.acknowledged_at IS NULL AND d.next_attempt_at <= CURRENT_TIMESTAMP
           AND e.expires_at > CURRENT_TIMESTAMP
         ORDER BY e.occurred_at,e.event_id LIMIT 1 FOR UPDATE OF d SKIP LOCKED")
        .fetch_optional(&mut *tx)).await.map_err(|_| ExportError::Timeout)?.map_err(ExportError::Database)?;
    let Some((service, id, payload, hash, occurred, expires, attempts)) = row else {
        return Ok(false);
    };
    let valid = (|| {
        let event: Event = serde_json::from_str(&payload).ok()?;
        let canonical = event.to_json().ok()?;
        let expiry = event.expires_at().ok()?;
        if event.service != service
            || event.event_id != id
            || event
                .occurred_at
                .timestamp_micros()
                .abs_diff(occurred.timestamp_micros())
                > 1
            || expiry
                .timestamp_micros()
                .abs_diff(expires.timestamp_micros())
                > 1
            || hex::encode(Sha256::digest(canonical)) != hash
        {
            return None;
        }
        Some((event, expiry))
    })();
    let Some((event, expiry)) = valid else {
        schedule_retry(&mut tx, &service, &id, attempts).await?;
        tx.commit().await.map_err(ExportError::Database)?;
        return Err(ExportError::InvalidStoredEvent);
    };
    let send_deadline = (deadline - std::time::Duration::from_secs(5))
        .min(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
    let acknowledged = if tokio::time::Instant::now() < send_deadline {
        match tokio::time::timeout_at(send_deadline, send(event)).await {
            Ok(Ok(Disposition::Accepted)) => true,
            Ok(Ok(Disposition::Expired)) => expiry <= Utc::now(),
            _ => false,
        }
    } else {
        false
    };
    if acknowledged {
        sqlx::query("UPDATE audit_delivery SET acknowledged_at=clock_timestamp(),attempts=CASE WHEN attempts<2147483647 THEN attempts+1 ELSE attempts END WHERE service=$1 AND event_id=$2")
            .bind(&service).bind(&id).execute(&mut *tx).await.map_err(ExportError::Database)?;
    } else {
        schedule_retry(&mut tx, &service, &id, attempts).await?;
    }
    tx.commit().await.map_err(ExportError::Database)?;
    if acknowledged {
        Ok(true)
    } else {
        Err(ExportError::Delivery)
    }
}

async fn schedule_retry(
    tx: &mut Transaction<'_, Postgres>,
    service: &str,
    id: &str,
    attempts: i32,
) -> Result<(), ExportError> {
    let delay = 1_i32 << attempts.clamp(0, 9);
    sqlx::query("UPDATE audit_delivery SET next_attempt_at=clock_timestamp()+$3*INTERVAL '1 second',attempts=CASE WHEN attempts<2147483647 THEN attempts+1 ELSE attempts END WHERE service=$1 AND event_id=$2")
        .bind(service).bind(id).bind(delay).execute(&mut **tx).await.map_err(ExportError::Database)?;
    Ok(())
}
