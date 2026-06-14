# etamong-audit (Rust)

Rust port of the etamong-lab cross-service audit contract (planning#193).
Sibling of [`shared/libs/audit-go`](https://gitlab.com/etamong-lab/shared/libs/audit-go).

## Stack rationale

Rust because this crate's consumers are Rust apps under the
[fleet-language-policy](https://gitlab.com/etamong-lab/planning/-/blob/main/wiki/concepts/fleet-language-policy.md)
SLO-bound exception (currently the `alert-ops`→`Kadabra` triage brain, planning#250).
The wire format mirrors the Go contract field-for-field so both produce the
same line on Loki and the "etamong-lab Audit" Grafana dashboard joins
across languages.

## Use

```rust
use etamong_audit::emit_audit;

let _ref = emit_audit(
    "kadabra",
    "ops@e.c",        // actor
    "verdict.set",    // action
    "run:42",         // target
    "ok",             // result
    serde_json::json!({ "verdict": "noise", "fix_ref": "infra!447" }),
);
```

## Wire format

```
{"level":"info","msg":"audit","app":"kadabra","ref":"a1b2c3d4",
 "actor":"ops@e.c","action":"verdict.set","target":"run:42","result":"ok",
 "detail":{"verdict":"noise","fix_ref":"infra!447"}}
```

`msg:"audit"` is the dashboard tag (vs `msg:"request failed"` of
[`etamong-httperr`](https://gitlab.com/etamong-lab/shared/libs/httperr-rs)).

## Related

- planning#250 — alert-ops split (this crate is Phase 1a)
- planning#193 — cross-service audit view (the contract)
- `wiki/concepts/fleet-language-policy.md`
- `shared/libs/audit-go` — Go canonical impl
- `shared/libs/httperr-rs` — Rust error (planning#188) companion
