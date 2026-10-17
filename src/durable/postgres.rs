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
