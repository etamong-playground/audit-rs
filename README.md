# etamong-audit (Rust)

> **About** — One of several shared libraries behind a personal homelab "fleet" of small apps (error handling · audit logging · encryption-at-rest · i18n · UI · …). Published to show the **design decisions** behind these cross-cutting concerns. It is authored and maintained with [Claude Code](https://www.anthropic.com/claude-code) (Anthropic's agentic CLI), not hand-written.
>
> **This is a public repository** — keep internal infrastructure details (hostnames, secret/Vault paths, private URLs, internal issue/MR references) out of code, comments, README, and commit messages.

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
