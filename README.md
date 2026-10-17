# etamong-audit (Rust)

> **About** — One of several small shared libraries used across a personal "fleet" of small apps (error handling · audit logging · encryption-at-rest · i18n · UI · …). Authored and maintained with [Claude Code](https://www.anthropic.com/claude-code) (Anthropic's agentic CLI). Each README documents the design rationale behind the library.
>
> **This is a public repository** — keep internal infrastructure details (hostnames, secret/Vault paths, private URLs, internal issue/MR references) out of code, comments, and commit messages.

Rust port of the etamong-playground cross-service audit contract.
Sibling of [`audit-go`](https://github.com/etamong-playground/audit-go).

## Stack rationale

Rust because this crate's consumers are Rust apps under a fleet-language-policy
SLO-bound exception (currently an internal triage service).
The wire format mirrors the Go contract field-for-field so both produce the
same structured log line, joinable across languages.

## Use

```rust
use etamong_audit::emit_audit;

let _ref = emit_audit(
    "myapp",
    "ops@e.c",        // actor
    "verdict.set",    // action
    "run:42",         // target
    "ok",             // result
    serde_json::json!({ "verdict": "noise" }),
);
```

## Wire format

```
{"level":"info","msg":"audit","app":"myapp","ref":"a1b2c3d4",
 "actor":"ops@e.c","action":"verdict.set","target":"run:42","result":"ok",
 "detail":{"verdict":"noise"}}
```

`msg:"audit"` is the dashboard tag (vs `msg:"request failed"` of
[`etamong-httperr`](https://github.com/etamong-playground/httperr-rs)).

## Related

- [`audit-go`](https://github.com/etamong-playground/audit-go) — Go canonical impl
- [`httperr-rs`](https://github.com/etamong-playground/httperr-rs) — Rust error companion

## Acknowledgements

Uses [`serde_json`](https://crates.io/crates/serde_json) (MIT OR Apache-2.0).

## License

MIT — see [LICENSE](LICENSE).

## Durable events

`durable::Event` is a separate validated contract for retained action history.
Operational `emit_audit` logging does not provide transactional persistence.
Producers allowlist changes and omit credentials, raw bodies and document contents.

```rust
use etamong_audit::durable::{actor_id, Actor, Event, new_id};

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
# Ok::<(), Box<dyn std::error::Error>>(())
```

`to_json` validates required fields, actor/result values, byte limits and the 64 KiB encoded payload limit.
It preserves Go-compatible JSON escaping, including the identity input's HTML and Unicode line-separator escapes.
Deserialization rejects unknown fields; callers must validate deserialized events before using them.
Expiry is two calendar years after UTC occurrence, clamping February 29 to February 28.
Occurrence years are limited to 1 through 9997 and leap seconds are rejected so both occurrence and expiry fit the shared wire representation.
Changing the dedicated identity key changes correlation; retain its version alongside the pseudonym.

The cross-language fixtures in `tests/fixtures/go-durable.json` were generated with `audit-go` commit `cb5129e3830a` using `durable.ActorID`, `Event.Validate` and `Event.ExpiresAt`.
They cover identity escaping, leap-day expiry, offset normalization and fractional seconds.

## PostgreSQL recording

Enable the optional `postgres` feature for the sqlx 0.8 adapter; logging-only consumers do not require a database dependency.
Apply `durable::postgres::SCHEMA` through the application's migration driver before serving traffic.
The enqueue trigger writes to `audit_delivery` in the event table's schema, even when a temporary table shadows that name.
For existing installations, apply the updated `audit_enqueue_event` function in a new forward migration; preserve already-applied migration checksums.
Updating the dependency alone does not replace a function installed from copied SQL.
The schema separates immutable canonical events from mutable delivery state and inserts the delivery row through a trigger in the same transaction.
Retained events reject updates, deletion and truncation; expired events permit deletion so bounded cleanup can remove them.
Use a restricted application database role because owners can disable database guards.

```rust,ignore
use etamong_audit::durable::postgres::record;

let mut tx = pool.begin().await?;
let outcome = async {
    sqlx::query("DELETE FROM records WHERE id = $1")
        .bind(record_id)
        .execute(&mut *tx)
        .await?;
    record(&mut tx, &event).await?;
    Ok::<(), Box<dyn std::error::Error>>(())
}.await;
match outcome {
    Ok(()) => tx.commit().await?,
    Err(error) => {
        tx.rollback().await?;
        return Err(error);
    }
}
```

`record` never commits or starts an independent transaction.
Any error, including validation errors before SQL execution, requires rolling back the business transaction.
Repeated local event IDs fail instead of silently attaching unrelated mutations to earlier history.
New records reject occurrences more than five minutes in the future or already expired; insertion preserves original occurrence and expiry.
Commit errors must not be reported as confirmed success; an uncertain commit requires application reconciliation using the stable event ID.

Run database tests against a disposable PostgreSQL database with `AUDIT_TEST_DATABASE_URL` and `cargo test --all-features`.
The tests create isolated schemas and fail when the database URL is missing, rather than silently skipping database coverage.

### Retrying delivery

`postgres::export_one(&pool, sender)` locks one eligible delivery row with `SKIP LOCKED` and returns `false` when none is ready.
The sender receives the original typed event and must authenticate the collector, validate its matching acknowledgement and return `Disposition::Accepted` only after central durable commit.
An `Expired` response is accepted only after the event's original expiry has passed.

```rust,ignore
use etamong_audit::durable::postgres::{export_one, Disposition};

let processed = export_one(&pool, |event| async move {
    // The application transport validates credentials and the matching event ID.
    collector.send(event).await
}).await?;
```

Failed sends persist exponential retry delays from one to 512 seconds, without changing the retained event or its expiry.
The sender future is cancelled after ten seconds and the whole pass after fifteen seconds, reserving five seconds from that deadline for acknowledgement or retry persistence; network clients must use cancellation-safe asynchronous I/O.
Cancellation or a lost commit acknowledgement can leave an uncertain central result, so the collector must deduplicate authenticated producer identity plus event ID.
Acknowledged and expired rows are excluded from subsequent delivery selection.
Stored payloads are decoded and canonically reserialized to verify their hash and indexed identity/timestamps before dispatch; invalid retained data receives bounded retry backoff and returns an error for operational reconciliation rather than being discarded.
Indexed timestamps allow one microsecond of database encoding precision loss; the canonical event retains the original timestamp.

### Expiry cleanup

`postgres::purge_expired(&mut tx, limit)` removes at most 1–10,000 expired events using their original expiry and cascades their delivery rows.
It returns only the deleted count and inclusive UTC occurrence/expiry ranges, never identities or payloads.
Unexpired events remain, whether or not central delivery has acknowledged them.

```rust,ignore
use etamong_audit::durable::{Change, postgres::{purge_expired, record}};

let mut tx = pool.begin().await?;
let summary = purge_expired(&mut tx, 1000).await?;
if summary.removed == 0 {
    tx.rollback().await?;
} else {
    summary_event.changes = [
        ("removed_count", summary.removed.to_string()),
        ("occurred_from", summary.occurred_from.unwrap().to_rfc3339()),
        ("occurred_to", summary.occurred_to.unwrap().to_rfc3339()),
        ("expired_from", summary.expired_from.unwrap().to_rfc3339()),
        ("expired_to", summary.expired_to.unwrap().to_rfc3339()),
    ].into_iter().map(|(field, after)| Change {
        field: field.into(), before: String::new(), after,
    }).collect();
    record(&mut tx, &summary_event).await?;
    tx.commit().await?;
}
```

The caller supplies the semantic cleanup event and owns the transaction deadline; every error must roll back that transaction, including failure to persist its summary.
Empty batches require no audit event, and a zero count under concurrent cleanup is not proof that the backlog is empty.
A commit error is not a confirmed deletion count; reconcile uncertain commits using the cleanup event's stable ID.
