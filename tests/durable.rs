use etamong_audit::durable::{actor_id, new_id, Event, MAX_EVENT_BYTES};
use serde_json::{json, Value};

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/go-durable.json")).unwrap()
}
fn event() -> Event {
    serde_json::from_value(fixtures()["events"][0]["event"].clone()).unwrap()
}

#[test]
fn matches_go_identity_and_expiration() {
    for fixture in fixtures()["identities"].as_array().unwrap() {
        assert_eq!(
            actor_id(
                fixture["key"].as_str().unwrap().as_bytes(),
                fixture["issuer"].as_str().unwrap(),
                fixture["subject"].as_str().unwrap()
            )
            .unwrap(),
            fixture["expected"]
        );
    }
    for fixture in fixtures()["events"].as_array().unwrap() {
        let event: Event = serde_json::from_value(fixture["event"].clone()).unwrap();
        event.validate().unwrap();
        assert_eq!(
            String::from_utf8(event.to_json().unwrap()).unwrap(),
            fixture["wire"]
        );
        assert_eq!(
            event.expires_at().unwrap(),
            fixture["expires_at"]
                .as_str()
                .unwrap()
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap()
        );
        assert_eq!(
            serde_json::from_slice::<Event>(&event.to_json().unwrap()).unwrap(),
            event
        );
    }
}

#[test]
fn rejects_invalid_contract_fields() {
    let baseline = serde_json::to_value(event()).unwrap();
    for (field, invalid) in [
        ("schema_version", json!(2)),
        ("event_id", json!(" ")),
        ("operation_id", json!("")),
        ("service", json!("x".repeat(513))),
        ("environment", json!("\0")),
        ("action", json!("")),
        ("target_type", json!("")),
        ("target_id", json!("é".repeat(257))),
        ("result", json!("success")),
        ("ref", json!("\0")),
        ("reason_code", json!("x".repeat(513))),
        ("occurred_at", json!("0001-01-01T00:00:00Z")),
    ] {
        let mut value = baseline.clone();
        value[field] = invalid;
        assert!(
            serde_json::from_value::<Event>(value)
                .unwrap()
                .validate()
                .is_err(),
            "accepted invalid {field}"
        );
    }
    for actor in [
        json!({"id":"x","type":"user"}),
        json!({"id":"","type":"job"}),
        json!({"id":"x","type":"admin"}),
    ] {
        for field in ["actor", "initiator"] {
            let mut value = baseline.clone();
            value[field] = actor.clone();
            assert!(serde_json::from_value::<Event>(value)
                .unwrap()
                .validate()
                .is_err());
        }
    }
    let mut value = baseline;
    value["secret"] = json!("must not accept arbitrary fields");
    assert!(serde_json::from_value::<Event>(value).is_err());
}

#[test]
fn enforces_change_and_encoded_size_boundaries() {
    let mut e = event();
    e.changes = vec![e.changes[0].clone(); 100];
    e.validate().unwrap();
    e.changes.push(e.changes[0].clone());
    assert!(e.validate().is_err());
    e.changes.truncate(1);
    e.changes[0].field = "".into();
    assert!(e.validate().is_err());
    e.changes[0].field = "label".into();
    e.changes[0].after = "x".into();
    let overhead = e.to_json().unwrap().len() - 1;
    e.changes[0].after = "x".repeat(MAX_EVENT_BYTES - overhead);
    assert_eq!(e.to_json().unwrap().len(), MAX_EVENT_BYTES);
    e.changes[0].after.push('x');
    assert!(e.validate().is_err());
    e.changes[0].after = "<".repeat(MAX_EVENT_BYTES / 6);
    assert!(
        e.validate().is_err(),
        "Go HTML escaping must count toward payload limit"
    );
}

#[test]
fn identity_rejects_missing_inputs_and_prevents_pair_ambiguity() {
    assert!(actor_id(&[0; 31], "issuer", "subject").is_err());
    assert!(actor_id(&[0; 32], "", "subject").is_err());
    assert!(actor_id(&[0; 32], "issuer", "").is_err());
    assert_ne!(
        actor_id(&[0; 32], "ab", "c").unwrap(),
        actor_id(&[0; 32], "a", "bc").unwrap()
    );
    assert_ne!(
        actor_id(&[0; 32], "issuer", "subject").unwrap(),
        actor_id(&[1; 32], "issuer", "subject").unwrap()
    );
}

#[test]
fn identifiers_and_optional_wire_fields() {
    let id = new_id();
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(id, new_id());
    let mut e = event();
    e.changes.clear();
    let data: Value = serde_json::from_slice(&e.to_json().unwrap()).unwrap();
    for field in ["ref", "reason_code", "changes", "initiator"] {
        assert!(data.get(field).is_none());
    }
}

#[test]
fn rejects_unrepresentable_expiry_and_leap_seconds() {
    for stamp in [
        "9998-01-01T00:00:00Z",
        "0000-01-01T00:00:00Z",
        "2016-12-31T23:59:60Z",
    ] {
        let mut e = event();
        e.occurred_at = stamp.parse().unwrap();
        assert!(e.validate().is_err(), "accepted {stamp}");
    }
}

#[test]
fn optional_nulls_match_go_empty_values() {
    let mut value = serde_json::to_value(event()).unwrap();
    for field in ["ref", "reason_code", "changes", "initiator"] {
        value[field] = Value::Null;
    }
    value["actor"]["type"] = json!("service");
    value["actor"]["key_id"] = Value::Null;
    let e: Event = serde_json::from_value(value).unwrap();
    e.validate().unwrap();
    assert!(e.changes.is_empty());
    assert!(e.actor.key_id.is_empty());
}
