//! The daemon's redaction matcher (PLAN §16b.6, decision S9).
//!
//! [`build`] makes a [`Redactor`] from:
//!
//! - each secret field of each secret in the store. The label is the
//!   secret name for a global secret (`[secret:gh]`), and
//!   `<project>/<name>` for a project secret (`[secret:web/deploy]`);
//! - each stored subscription API key in
//!   `<state>/credentials/subscriptions/<name>.json`, with the label
//!   `subscription/<name>` (`[secret:subscription/zai]`).
//!
//! The decrypted values live only for the build, in zeroizing buffers. The
//! matcher keeps its own copy of the patterns: it must, to find them.
//!
//! The daemon keeps one matcher in `Db` (`crate::db::Db::redactor`). It
//! builds it at the start, after the migration of the old credential files
//! ([`rebuild`]), and again after each change:
//!
//! - each successful write call of `/api/secrets` and `/api/credentials`
//!   ([`after_change`], a route layer in `crate::api`);
//! - the delete of a project (its secrets go with it);
//! - a stored subscription API key, and the delete of a subscription.
//!
//! `terrariumd secrets rotate-key` runs with no daemon; the values do not
//! change, and the next start builds the matcher again.
//!
//! Where the daemon applies the matcher:
//!
//! - At write time, in the store: [`crate::db::Tx::redact`] and
//!   [`crate::db::Tx::emit`] (events, trigger payloads), and
//!   [`crate::db::Tx::audit`] (audit details). The database never holds a
//!   known value in these records.
//! - At read time, for the files that a harness or a runner writes: the
//!   transcripts and the session view, the runner logs, and the PTY screen
//!   snapshots ([`current`]).
//!
//! The live terminal stream to the operator is not redacted.

use std::path::Path;

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use rusqlite::Connection;
use terrarium_core::redact::Redactor;
use terrarium_core::transcript::Block;
use zeroize::Zeroizing;

use super::{GLOBAL, key, list, open};
use crate::api::{AppState, Shared};
use crate::error::ApiResult;

/// The label prefix of a subscription API key.
pub const SUBSCRIPTION_PREFIX: &str = "subscription/";

/// The label of a secret in `[secret:<label>]`: the name for a global
/// secret, `<project>/<name>` for a project secret.
pub fn label(scope: &str, name: &str) -> String {
    if scope == GLOBAL {
        name.to_owned()
    } else {
        format!("{scope}/{name}")
    }
}

/// The `(label, value)` pairs of each secret field of each secret. A
/// secret with no sealed value yet adds nothing.
fn secret_entries(
    conn: &Connection,
    state_dir: &Path,
) -> ApiResult<Vec<(String, Zeroizing<String>)>> {
    let infos = list(conn, None)?;
    if infos.is_empty() {
        return Ok(Vec::new());
    }
    let key = key(state_dir)?;
    let mut out = Vec::new();
    for info in infos {
        let opened = match open(conn, &key, &info.scope, &info.name) {
            Ok(Some(o)) => o,
            Ok(None) => continue,
            Err(e) => {
                // A secret with no value yet, or one that does not open:
                // the others still count.
                tracing::debug!(scope = %info.scope, name = %info.name,
                                "redaction: skip a secret: {}", e.message);
                continue;
            }
        };
        let l = label(&info.scope, &info.name);
        for (_, v) in opened.values {
            out.push((l.clone(), v));
        }
    }
    Ok(out)
}

/// The `(label, key)` pairs of the stored subscription API keys.
fn subscription_entries(state_dir: &Path) -> Vec<(String, Zeroizing<String>)> {
    let dir = super::credentials_dir(state_dir).join("subscriptions");
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let file = entry.file_name();
        let Some(name) = file
            .to_str()
            .filter(|n| !n.starts_with('.'))
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if let Some(k) = crate::login_session::read_key(state_dir, name) {
            out.push((
                format!("{SUBSCRIPTION_PREFIX}{name}"),
                Zeroizing::new(k.key),
            ));
        }
    }
    out
}

/// Build the matcher from the store and the subscription keys. A value
/// shorter than [`terrarium_core::redact::MIN_LEN`] bytes is skipped.
pub fn build(conn: &Connection, state_dir: &Path) -> ApiResult<Redactor> {
    let mut entries = secret_entries(conn, state_dir)?;
    entries.extend(subscription_entries(state_dir));
    let redactor = Redactor::new(entries.iter().map(|(l, v)| (l.clone(), v.as_bytes())))
        .map_err(|e| crate::error::ApiError::internal(e.to_string()))?;
    // `entries` drops here and zeroes each value.
    Ok(redactor)
}

/// Build the matcher and swap it into the daemon's handle. The build runs
/// under the database lock, so two rebuilds swap in the order of the
/// writes they follow. A failure keeps the old matcher and logs it.
pub async fn rebuild(state: &AppState) {
    let handle = state.db.redactor().clone();
    let dir = state.state_dir.clone();
    let result = state
        .db
        .read(move |c| {
            let r = build(c, &dir)?;
            let n = r.pattern_count();
            handle.replace(r);
            Ok(n)
        })
        .await;
    match result {
        Ok(n) => tracing::debug!(patterns = n, "redaction matcher built"),
        Err(e) => tracing::error!("cannot build the redaction matcher: {}", e.message),
    }
}

/// Redact `s` in place. No allocation when nothing matches.
fn redact_in_place(r: &Redactor, s: &mut String) {
    if let std::borrow::Cow::Owned(new) = r.redact_str(s) {
        *s = new;
    }
}

/// Redact the text of a transcript block: the text, the turn parts, the
/// tool call (input and summary), and the tool result.
pub fn redact_block(r: &Redactor, b: &mut Block) {
    if r.is_empty() {
        return;
    }
    if let Some(t) = &mut b.text {
        redact_in_place(r, t);
    }
    for p in &mut b.parts {
        redact_in_place(r, &mut p.body);
    }
    if let Some(tool) = &mut b.tool {
        redact_in_place(r, &mut tool.summary);
        tool.input = crate::db::redact_json(r, std::mem::take(&mut tool.input));
    }
    if let Some(res) = &mut b.result {
        redact_in_place(r, &mut res.output);
    }
}

/// A route layer for `/api/secrets` and `/api/credentials`: after each
/// successful call that is not a read, build the matcher again.
pub async fn after_change(State(state): State<Shared>, req: Request, next: Next) -> Response {
    let write = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let resp = next.run(req).await;
    if write && resp.status().is_success() {
        rebuild(&state).await;
    }
    resp
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::secrets::{Kind, MasterKey, NewSecret, PutMode, SecretInput, crypto, put};

    fn put_token(c: &Connection, k: &MasterKey, scope: &str, name: &str, value: &str) {
        let input: SecretInput =
            serde_json::from_value(serde_json::json!({"fields": {"value": value}})).unwrap();
        put(
            c,
            k,
            PutMode::Upsert,
            &NewSecret {
                scope,
                name,
                kind: Kind::Token,
                input: &input,
                by: "operator",
                strict: true,
            },
        )
        .unwrap();
    }

    #[test]
    fn builds_from_secrets_and_subscription_keys() {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::secrets::tests::db();
        let k = MasterKey::generate();
        crypto::write_key(&crypto::key_path(dir.path()), &k).unwrap();
        put_token(&c, &k, GLOBAL, "gh", "ghp_globalvalue123");
        put_token(&c, &k, "p", "deploy", "projectvalue4567");
        put_token(&c, &k, GLOBAL, "short", "abc");
        crate::login_session::store_key(
            dir.path(),
            "zai",
            &crate::login_session::ApiKey {
                provider: "zai".into(),
                key: "zai-key-abcdefgh".into(),
                plan: None,
                base_url: None,
                models: Vec::new(),
            },
        )
        .unwrap();

        let r = build(&c, dir.path()).unwrap();
        assert_eq!(
            r.redact_str("a ghp_globalvalue123 b projectvalue4567 c zai-key-abcdefgh abc"),
            "a [secret:gh] b [secret:p/deploy] c [secret:subscription/zai] abc"
        );
    }

    #[test]
    fn no_secret_is_the_empty_matcher_without_a_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::secrets::tests::db();
        assert!(build(&c, dir.path()).unwrap().is_empty());
    }
}
