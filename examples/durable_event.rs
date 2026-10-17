fn main() -> Result<(), Box<dyn std::error::Error>> {
    use etamong_audit::durable::{actor_id, new_id, Actor, Event};

    let key = [7_u8; 32]; // Supply a dedicated secret key in the application.
    let event = Event {
        schema_version: 1,
        event_id: new_id(),
        operation_id: new_id(),
        service: "example".into(),
        environment: "test".into(),
        occurred_at: chrono::Utc::now().fixed_offset(),
        actor: Actor {
            id: actor_id(&key, "https://identity.example", "verified-subject")?,
            kind: "user".into(),
            key_id: "v1".into(),
        },
        initiator: None,
        action: "record.delete".into(),
        target_type: "record".into(),
        target_id: "42".into(),
        result: "ok".into(),
        r#ref: String::new(),
        reason_code: String::new(),
        changes: Vec::new(),
    };
    let payload = event.to_json()?;
    let expires_at = event.expires_at()?;
    println!("{}", String::from_utf8(payload)?);
    assert!(expires_at > event.occurred_at);
    Ok(())
}
