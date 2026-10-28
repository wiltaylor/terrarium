//! The store side of the unified transcript (`docs/api.md` §6.6): the
//! session file that a harness reported, and the delivery log of typed
//! turns.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use terrarium_core::transcript::Origin;

use crate::db::Tx;
use crate::error::ApiResult;

/// The rows of the delivery log that each agent keeps.
pub const KEEP_DELIVERIES: i64 = 2000;

/// The hash of a typed text in the delivery log: SHA-256 (hex) of the text
/// with each run of whitespace made one space, and no space at the ends.
/// The harness can keep the text with other line ends. The escape of a
/// turn that starts with `/` or `!` (`terrarium_core::adapter::TURN_ESCAPE`)
/// does not count: the delivery log has the text as sent, and the
/// transcript has the text as typed.
pub fn text_hash(text: &str) -> String {
    let text = text.trim_start_matches(terrarium_core::adapter::TURN_ESCAPE);
    let norm = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let digest = Sha256::digest(norm.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn origin_str(o: Origin) -> &'static str {
    match o {
        Origin::Operator => "operator",
        Origin::AgentMailPointer => "agent_mail_pointer",
        Origin::System => "system",
        Origin::User => "user",
    }
}

fn origin_from(s: &str) -> Option<Origin> {
    Some(match s {
        "operator" => Origin::Operator,
        "agent_mail_pointer" => Origin::AgentMailPointer,
        "system" => Origin::System,
        "user" => Origin::User,
        _ => return None,
    })
}

/// Record that the daemon types `text` into `agent` now, as `origin`. Keeps
/// the newest [`KEEP_DELIVERIES`] rows of the agent.
pub fn record_delivery(t: &mut Tx, agent: &str, origin: Origin, text: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO deliveries (agent_id, origin, hash, at) VALUES (?1, ?2, ?3, ?4)",
        params![agent, origin_str(origin), text_hash(text), crate::db::now()],
    )?;
    t.execute(
        "DELETE FROM deliveries WHERE agent_id = ?1 AND id <= (
             SELECT id FROM deliveries WHERE agent_id = ?1
             ORDER BY id DESC LIMIT 1 OFFSET ?2)",
        params![agent, KEEP_DELIVERIES],
    )?;
    Ok(())
}

/// The origin of each hash in `hashes` that the delivery log of `agent`
/// has (the newest row wins).
pub fn delivery_origins(
    conn: &Connection,
    agent: &str,
    hashes: &[String],
) -> ApiResult<HashMap<String, Origin>> {
    let mut out = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT origin FROM deliveries WHERE agent_id = ?1 AND hash = ?2
         ORDER BY id DESC LIMIT 1",
    )?;
    for h in hashes {
        if out.contains_key(h) {
            continue;
        }
        let o: Option<String> = stmt.query_row(params![agent, h], |r| r.get(0)).optional()?;
        if let Some(o) = o.as_deref().and_then(origin_from) {
            out.insert(h.clone(), o);
        }
    }
    Ok(out)
}

/// The `transcript_path` and `harness_session` of the newest
/// `session_started` event of a session.
pub fn reported_transcript(
    conn: &Connection,
    session: &str,
) -> ApiResult<Option<(Option<String>, Option<String>)>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM events WHERE session_id = ?1 AND kind = 'session_started'
             ORDER BY seq DESC LIMIT 1",
            [session],
            |r| r.get(0),
        )
        .optional()?;
    Ok(payload.map(|p| {
        let v: serde_json::Value = serde_json::from_str(&p).unwrap_or_default();
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
        (get("transcript_path"), get("harness_session"))
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hash_ignores_whitespace_shape() {
        assert_eq!(text_hash("a  b\r\nc "), text_hash("a b c"));
        assert_ne!(text_hash("a b"), text_hash("a c"));
        assert_eq!(text_hash("x").len(), 64);
        // The escape of a command turn does not change the hash.
        assert_eq!(text_hash("\u{200B}  /plan now"), text_hash("/plan now"));
    }
}
