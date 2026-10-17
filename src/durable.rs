#[cfg(feature = "postgres")]
pub mod postgres;

use chrono::{DateTime, Datelike, FixedOffset, SecondsFormat, Timelike, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{error::Error, fmt};

pub const MAX_EVENT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub key_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub field: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub before: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub after: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub schema_version: u32,
    pub event_id: String,
    pub operation_id: String,
    pub service: String,
    pub environment: String,
    #[serde(serialize_with = "go_timestamp")]
    pub occurred_at: DateTime<FixedOffset>,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<Actor>,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub result: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub r#ref: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub reason_code: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidEvent(&'static str);

impl fmt::Display for InvalidEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl Error for InvalidEvent {}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Requires a dedicated key of at least 32 bytes and a verified issuer/subject pair.
/// Keep the key version with the resulting identity; rotation changes correlation.
pub fn actor_id(key: &[u8], issuer: &str, subject: &str) -> Result<String, InvalidEvent> {
    if key.len() < 32 || issuer.is_empty() || subject.is_empty() {
        return Err(InvalidEvent(
            "identity requires a 32-byte key, issuer and subject",
        ));
    }
    let encoded = go_json(&[issuer, subject])?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key).map_err(|_| InvalidEvent("invalid identity key"))?;
    mac.update(&encoded);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

impl Event {
    /// Two calendar years after occurrence in UTC, clamping February 29.
    /// Dates outside the wire format's four-digit year range are rejected.
    pub fn expires_at(&self) -> Result<DateTime<Utc>, InvalidEvent> {
        let t = self.occurred_at.with_timezone(&Utc);
        if !(1..=9997).contains(&t.year()) || t.nanosecond() >= 1_000_000_000 {
            return Err(InvalidEvent("occurrence outside supported year range"));
        }
        let t = if t.month() == 2 && t.day() == 29 {
            t.with_day(28).ok_or(InvalidEvent("invalid occurrence"))?
        } else {
            t
        };
        t.with_year(t.year() + 2)
            .ok_or(InvalidEvent("invalid expiry"))
    }

    pub fn validate(&self) -> Result<(), InvalidEvent> {
        if self.schema_version != 1
            || (self.occurred_at.timestamp() == -62_135_596_800
                && self.occurred_at.nanosecond() == 0)
        {
            return Err(InvalidEvent(
                "event requires schema version 1 and occurrence time",
            ));
        }
        self.expires_at()?;
        for text in [
            &self.event_id,
            &self.operation_id,
            &self.service,
            &self.environment,
            &self.action,
            &self.target_type,
            &self.target_id,
        ] {
            valid_text(text, true)?;
        }
        valid_actor(&self.actor)?;
        if let Some(actor) = &self.initiator {
            valid_actor(actor)?;
        }
        if !["ok", "denied", "error", "pending", "unknown"].contains(&self.result.as_str()) {
            return Err(InvalidEvent("invalid audit result"));
        }
        valid_text(&self.r#ref, false)?;
        valid_text(&self.reason_code, false)?;
        if self.changes.len() > 100 {
            return Err(InvalidEvent("too many audit changes"));
        }
        for change in &self.changes {
            valid_text(&change.field, true)?;
        }
        if go_json(self)?.len() > MAX_EVENT_BYTES {
            return Err(InvalidEvent("event exceeds 64 KiB"));
        }
        Ok(())
    }

    /// Validates before returning a bounded payload suitable for persistence or transport.
    pub fn to_json(&self) -> Result<Vec<u8>, InvalidEvent> {
        self.validate()?;
        go_json(self)
    }
}

fn valid_actor(actor: &Actor) -> Result<(), InvalidEvent> {
    valid_text(&actor.id, true)?;
    valid_text(&actor.key_id, false)?;
    match actor.kind.as_str() {
        "user" if !actor.key_id.is_empty() => Ok(()),
        "service" | "job" | "anonymous" => Ok(()),
        _ => Err(InvalidEvent(
            "invalid actor type or missing user key version",
        )),
    }
}

fn valid_text(value: &str, required: bool) -> Result<(), InvalidEvent> {
    if value.len() > 512 || value.contains('\0') || (required && value.trim().is_empty()) {
        return Err(InvalidEvent("invalid audit text"));
    }
    Ok(())
}

fn go_json(value: &impl Serialize) -> Result<Vec<u8>, InvalidEvent> {
    let json = serde_json::to_string(value).map_err(|_| InvalidEvent("invalid event encoding"))?;
    Ok(json
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .into_bytes())
}

fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn go_timestamp<S: serde::Serializer>(
    time: &DateTime<FixedOffset>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let raw = time.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let (date, rest) = raw
        .split_once('.')
        .ok_or_else(|| serde::ser::Error::custom("invalid timestamp"))?;
    let fraction = rest[..9].trim_end_matches('0');
    let suffix = &rest[9..];
    let text = if fraction.is_empty() {
        format!("{date}{suffix}")
    } else {
        format!("{date}.{fraction}{suffix}")
    };
    serializer.serialize_str(&text)
}
