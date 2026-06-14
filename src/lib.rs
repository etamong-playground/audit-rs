//! etamong-lab cross-service audit contract (planning#193), Rust port.
//!
//! One stderr line per audit event: `{"level":"info","msg":"audit","app",
//! "ref","actor","action","target","result","detail"}`. `msg:"audit"` is the
//! distinguishing tag the "etamong-lab Audit" Grafana dashboard groups on
//! (vs `msg:"request failed"` of [`etamong-httperr`](
//! https://gitlab.com/etamong-lab/shared/libs/httperr-rs)).
//!
//! `ref` is a fresh 8-hex token per event — joining to a per-request id
//! (planning#201) is a separate follow-up.
//!
//! Field meanings:
//! - `actor`   — operator email (or `"-"` for system).
//! - `action`  — namespaced verb (`alert.ack`, `triage.comment`,
//!               `link.create`).
//! - `target`  — resource id (`fp-abc123`, `link:42`).
//! - `result`  — outcome (`ok`, `denied`, `not-found`, …).
//! - `detail`  — arbitrary JSON for context (input snippet, prior state, …).

use std::hash::{BuildHasher, Hasher};

/// Emits one audit line to stderr. Returns the generated `ref` so callers can
/// quote it back to operators or correlate per-request.
pub fn emit_audit(
    app: &str,
    actor: &str,
    action: &str,
    target: &str,
    result: &str,
    detail: serde_json::Value,
) -> String {
    let r = new_ref();
    let line = serde_json::json!({
        "level": "info",
        "msg": "audit",
        "app": app,
        "ref": r,
        "actor": if actor.is_empty() { "-" } else { actor },
        "action": action,
        "target": target,
        "result": result,
        "detail": detail,
    });
    eprintln!("{line}");
    r
}

/// Same `ref` generator as [`etamong-httperr`]. Duplicated here so
/// `audit-rs` stays self-contained (no inter-crate dependency); both crates
/// produce 8-hex strings drawn from the stdlib random-seeded hasher and the
/// values are interchangeable.
fn new_ref() -> String {
    let n = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{:08x}", n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_audit_returns_8_hex_ref() {
        let r = emit_audit(
            "kadabra",
            "ops@e.c",
            "verdict.set",
            "run:42",
            "ok",
            serde_json::json!({ "verdict": "noise" }),
        );
        assert_eq!(r.len(), 8);
        assert!(r.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn new_ref_is_8_hex_chars() {
        for _ in 0..100 {
            let r = new_ref();
            assert_eq!(r.len(), 8);
            assert!(r.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}
