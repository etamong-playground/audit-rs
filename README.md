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
