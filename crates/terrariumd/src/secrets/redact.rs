//! The daemon's redaction matcher (PLAN §16b.6, decision S9).
//!
//! [`build`] makes a [`Redactor`] from:
//!
//! - each secret field of each secret in the store. The label is the
//!   secret name for a global secret (`[secret:gh]`), and
//!   `<project>/<name>` for a project secret (`[secret:web/deploy]`);
//! - each stored subscription API key in
//!   `<state>/credentials/subscriptions/<name>.json`, with the label
//!   `subscription/<name>` (`[secret:subscription/zai]`);
//! - each string value of each Grok Build login of an xAI plan account,
//!   `<state>/credentials/accounts/<name>/store/grok/auth.json`
//!   ([`crate::accounts::grok_secret_values`]), with the label
//!   `subscription/<name>`. Grok refreshes that file itself: the daemon
//!   builds the matcher again after a Grok login and after a verify of the
//!   row, so a refreshed token joins it then (TER-1191).
//!
//! - each key that the daemon dropped from a row that became an xai plan
//!   row ([`retain_dropped`], TER-1239), for the life of the daemon: an
//!   agent that started with the key (a pi agent) may still run and print
//!   it. A restart forgets these keys.
//!
//! The decrypted values live only for the build, in zeroizing buffers. The
//! matcher keeps its own copy of the patterns, for the life of the daemon:
//! it must, to find them. That copy is not zeroized.
//!
//! The matcher leaves out a value (or one line of a value) that
//! [`keep`] refuses: a value that looks like an identifier of the records
//! (a ULID, a record word, a name of a project, an agent, a target, ...),
//! or a weak value shorter than 16 bytes. Such a value matches ordinary
//! text, so a matcher with it blinds records (PLAN §16b.6, S4). [`check_value`]
//! refuses most of these values at a put; a weak login password or
//! passphrase is stored, but not redacted.
//!
//! The label of a match shows the name only to a view that may see it
//! (`crate::db::writer_view`, [`view_of`]); the others see `[secret]`.
//!
//! The daemon keeps one matcher in `Db` (`crate::db::Db::redactor`). It
//! builds it at the start, after the migration of the old credential files
//! ([`rebuild`]), and again after each change:
//!
//! - a write that adds or changes a value (a secret put, a generated key, a
//!   stored subscription API key) builds it inside its transaction, before
//!   the commit ([`crate::db::Tx::rebuild_redactor`]), so no other write
//!   stores the value with the old matcher (S11);
//! - a delete (a secret, a project, a subscription) builds it after the
//!   commit ([`rebuild`]): until then the old matcher redacts more, never
//!   less.
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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use rusqlite::Connection;
use terrarium_core::redact::{self as core, Entry, Judgment, Redactor, View};
use terrarium_core::transcript::Block;
use zeroize::Zeroizing;

use super::{GLOBAL, Kind, key, list, open};
use crate::actor::Actor;
use crate::api::AppState;
use crate::error::{ApiError, ApiResult};

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

/// The entries of each secret field of each secret. The scope of an entry
/// is the scope of its secret. A secret with no sealed value yet adds
/// nothing.
fn secret_entries(conn: &Connection, state_dir: &Path) -> ApiResult<Vec<Entry<Zeroizing<String>>>> {
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
            out.push(Entry {
                label: l.clone(),
                scope: Some(info.scope.clone()),
                value: v,
            });
        }
    }
    Ok(out)
}

/// The entries of the stored subscription API keys. Their scope is
/// `global`: a project agent sees `[secret]`.
fn subscription_entries(state_dir: &Path) -> Vec<Entry<Zeroizing<String>>> {
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
            for value in k.secret_values() {
                out.push(Entry {
                    label: format!("{SUBSCRIPTION_PREFIX}{name}"),
                    scope: Some(GLOBAL.to_owned()),
                    value: Zeroizing::new(value),
                });
            }
        }
    }
    out
}

/// The names of this instance, in lower case: projects, agents (also as
/// `<project>/<name>`), repos, hosts, targets, subscriptions, schedules,
/// templates, event sources, and secrets. A secret value equal to one of
/// them is an identifier.
pub fn record_names(conn: &Connection) -> ApiResult<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM projects
         UNION SELECT name FROM agents
         UNION SELECT project || '/' || name FROM agents
         UNION SELECT name FROM repos
         UNION SELECT project || '/' || name FROM repos
         UNION SELECT name FROM hosts
         UNION SELECT name FROM targets
         UNION SELECT name FROM subscriptions
         UNION SELECT name FROM schedules
         UNION SELECT name FROM agent_templates
         UNION SELECT name FROM event_sources
         UNION SELECT name FROM secrets",
    )?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.into_iter().map(|n| n.to_lowercase()).collect())
}

/// True when the matcher may hold `pattern` (one raw pattern of a value,
/// [`core::raw_patterns`]): [`core::judge`] finds it safe, and it is not a
/// name of this instance.
pub fn keep(names: &HashSet<String>, pattern: &[u8]) -> bool {
    if core::judge(pattern) != Judgment::Ok {
        return false;
    }
    match std::str::from_utf8(pattern) {
        Ok(s) => !names.contains(&s.trim().to_lowercase()),
        Err(_) => true,
    }
}

/// True for a secret field that a person chooses: the password of a
/// `login`, the passphrase of an `ssh_key`, each field of a `generic`.
/// A weak value there is stored, but not redacted. The other fields
/// (tokens, API keys, private keys) are made by a machine: a weak value
/// there is refused.
fn chosen_by_a_person(kind: Kind, field: &str) -> bool {
    matches!(
        (kind, field),
        (Kind::Login, "password") | (Kind::SshKey, "passphrase") | (Kind::Generic, _)
    )
}

/// Check the value of one secret field at a put (PLAN §16b.6, S4).
///
/// - A value that looks like an identifier (a ULID, an actor label, a
///   record word, a name of this instance) is refused, for each kind: it
///   is not a secret, and redaction would blind each record that holds it.
/// - A weak value (shorter than 16 bytes, and one word or name, or low
///   entropy) is refused for a token or a key. For a field that a person
///   chooses (a login password, a passphrase, a `generic` field), it is
///   stored, and the answer is a warning: redaction does not hide it.
///
/// The answer is the warnings.
pub fn check_value(
    names: &HashSet<String>,
    kind: Kind,
    field: &str,
    value: &str,
) -> ApiResult<Vec<String>> {
    let mut warnings = Vec::new();
    for pattern in core::raw_patterns(value.as_bytes()) {
        let text = String::from_utf8_lossy(pattern);
        let why = match core::judge(pattern) {
            Judgment::Identifier(why) => Some(why),
            Judgment::Ok if names.contains(&text.trim().to_lowercase()) => {
                Some("it is a name of a project, an agent, a target, or another record here")
            }
            Judgment::Ok => None,
            Judgment::Weak(why) => {
                if chosen_by_a_person(kind, field) {
                    warnings.push(format!(
                        "the value of `{field}` is weak ({why}): it is stored, but redaction does \
                         not hide it in records, because a weak value matches ordinary text"
                    ));
                    continue;
                }
                return Err(ApiError::invalid(format!(
                    "the value of `{field}` is weak ({why}); the value of a secret of kind \
                     `{kind}` is random: 16 bytes or more, or not one word, name, or number"
                )));
            }
        };
        if let Some(why) = why {
            return Err(ApiError::invalid(format!(
                "the value of `{field}` cannot be a secret: {why}. Redaction would replace it \
                 in each record that holds it"
            )));
        }
    }
    Ok(warnings)
}

/// The entries of the Grok Build logins in the plan accounts (TER-1191).
/// Their scope is `global`, as for the subscription keys.
fn grok_entries(state_dir: &Path) -> Vec<Entry<Zeroizing<String>>> {
    let mut out = Vec::new();
    for (name, values) in crate::accounts::grok_secret_values(state_dir) {
        for value in values {
            out.push(Entry {
                label: format!("{SUBSCRIPTION_PREFIX}{name}"),
                scope: Some(GLOBAL.to_owned()),
                value: Zeroizing::new(value),
            });
        }
    }
    out
}

/// The values of the dropped subscription keys, by state dir (one daemon,
/// or one test daemon), with the subscription name.
type Dropped = HashMap<PathBuf, Vec<(String, Zeroizing<String>)>>;

static DROPPED: LazyLock<Mutex<Dropped>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Keep the values of a subscription key that the daemon drops (TER-1239,
/// rev-1205b): the key file goes, but an agent that started with the key
/// may still run, so its output stays redacted until the daemon ends. Call
/// it before the key file goes; the next build holds the values.
pub fn retain_dropped(state_dir: &Path, name: &str, values: Vec<String>) {
    let mut map = DROPPED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let list = map.entry(state_dir.to_owned()).or_default();
    for v in values {
        if !list.iter().any(|(n, k)| n == name && k.as_str() == v) {
            list.push((name.to_owned(), Zeroizing::new(v)));
        }
    }
}

/// The entries of the dropped subscription keys of `state_dir`, with the
/// label and scope of a stored key.
fn dropped_entries(state_dir: &Path) -> Vec<Entry<Zeroizing<String>>> {
    let map = DROPPED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.get(state_dir)
        .into_iter()
        .flatten()
        .map(|(name, value)| Entry {
            label: format!("{SUBSCRIPTION_PREFIX}{name}"),
            scope: Some(GLOBAL.to_owned()),
            value: value.clone(),
        })
        .collect()
}

/// Build the matcher from the store and the subscription keys. A value
/// shorter than [`terrarium_core::redact::MIN_LEN`] bytes is skipped, and
/// so is each pattern that [`keep`] refuses.
pub fn build(conn: &Connection, state_dir: &Path) -> ApiResult<Redactor> {
    let names = record_names(conn)?;
    let mut entries = secret_entries(conn, state_dir)?;
    entries.extend(subscription_entries(state_dir));
    entries.extend(grok_entries(state_dir));
    entries.extend(dropped_entries(state_dir));
    let redactor = Redactor::build(
        entries.iter().map(|e| Entry {
            label: e.label.clone(),
            scope: e.scope.clone(),
            value: e.value.as_bytes(),
        }),
        |p| keep(&names, p),
    )
    .map_err(|e| ApiError::internal(e.to_string()))?;
    // `entries` drops here and zeroes each value.
    Ok(redactor)
}

/// Build the matcher and swap it into the daemon's handle: at the start,
/// and after a delete. The build runs under the database lock, so two
/// rebuilds swap in the order of the writes they follow. A failure keeps
/// the old matcher and logs it. A write that adds a value uses
/// [`crate::db::Tx::rebuild_redactor`] instead.
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

/// The labels that `actor` sees in a text that the daemon redacts when it
/// shows it (a transcript, a screen, an upload): each name for the
/// operator and a sysadmin; the names of the own project for another
/// agent, and `[secret]` for the others (PLAN §16b.6, S5).
pub fn view_of(actor: &Actor) -> View {
    match actor {
        Actor::Operator => View::All,
        Actor::Agent(a) if a.reads_all() => View::All,
        Actor::Agent(a) => View::Scope(a.project.clone()),
    }
}

/// Redact `s` in place. No allocation when nothing matches.
fn redact_in_place(r: &Redactor, view: &View, s: &mut String) {
    if let (std::borrow::Cow::Owned(new), _) = r.redact_str_as(s, view) {
        *s = new;
    }
}

/// Redact the text of a transcript block for `view`: the text, the turn
/// parts, the tool call (input and summary), and the tool result.
pub fn redact_block(r: &Redactor, view: &View, b: &mut Block) {
    if r.is_empty() {
        return;
    }
    if let Some(t) = &mut b.text {
        redact_in_place(r, view, t);
    }
    for p in &mut b.parts {
        redact_in_place(r, view, &mut p.body);
    }
    if let Some(tool) = &mut b.tool {
        redact_in_place(r, view, &mut tool.summary);
        tool.input = crate::db::redact_json_values(r, view, std::mem::take(&mut tool.input));
    }
    if let Some(res) = &mut b.result {
        redact_in_place(r, view, &mut res.output);
    }
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
                key_id: None,
                cloud: None,
            },
        )
        .unwrap();

        let r = build(&c, dir.path()).unwrap();
        assert_eq!(
            r.redact_str("a ghp_globalvalue123 b projectvalue4567 c zai-key-abcdefgh abc"),
            "a [secret:gh] b [secret:p/deploy] c [secret:subscription/zai] abc"
        );
    }

    /// TER-1191: the matcher holds each token of a Grok Build login of an
    /// xAI plan account, and a refreshed token after a rebuild. The keys
    /// and short fields of the file (`expires_at`, a type word) stay
    /// visible.
    #[test]
    fn grok_logins_and_refreshed_tokens_are_in_the_matcher() {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::secrets::tests::db();
        let old = r#"{"session": {"access_token": "xai-at-Q7w9Zk2LmP4rT8vB", "refresh_token": "xai-rt-H3nJ6sD1fG5kL0pX", "token_type": "Bearer"}, "expires_at": "2026-11-13T12:00:00Z"}"#;
        crate::accounts::write_credential(
            dir.path(),
            "x-super",
            terrarium_core::HarnessId::Grok,
            old.as_bytes(),
        )
        .unwrap();
        let r = build(&c, dir.path()).unwrap();
        assert_eq!(
            r.redact_str(
                "at=xai-at-Q7w9Zk2LmP4rT8vB rt=xai-rt-H3nJ6sD1fG5kL0pX Bearer access_token"
            ),
            "at=[secret:subscription/x-super] rt=[secret:subscription/x-super] Bearer access_token"
        );
        // A project agent sees no name.
        assert_eq!(
            r.redact_str_as("xai-rt-H3nJ6sD1fG5kL0pX", &View::Scope("p".into()))
                .0,
            "[secret]"
        );
        // Grok refreshes its file: the next build has the new token.
        let new = r#"{"session": {"access_token": "xai-at-N4eW8tOk3nV6yU2c", "refresh_token": "xai-rt-H3nJ6sD1fG5kL0pX"}}"#;
        crate::accounts::write_credential(
            dir.path(),
            "x-super",
            terrarium_core::HarnessId::Grok,
            new.as_bytes(),
        )
        .unwrap();
        let stale = r.redact_str("xai-at-N4eW8tOk3nV6yU2c").into_owned();
        assert_eq!(
            stale, "xai-at-N4eW8tOk3nV6yU2c",
            "the old matcher does not know it"
        );
        let r = build(&c, dir.path()).unwrap();
        assert_eq!(
            r.redact_str("xai-at-N4eW8tOk3nV6yU2c"),
            "[secret:subscription/x-super]"
        );
        // The daemon replaced the file: the old token stays (TER-1257).
        assert_eq!(
            r.redact_str("xai-at-Q7w9Zk2LmP4rT8vB"),
            "[secret:subscription/x-super]"
        );
        // A transcript block: the text, the tool input, and the result.
        use terrarium_core::transcript::{BlockKind, BlockRole, ToolCall, ToolResult};
        let mut b = Block::new(
            "m:1",
            BlockRole::Assistant,
            BlockKind::ToolCall,
            terrarium_core::HarnessId::Grok,
            1,
            None,
        );
        b.text = Some("token xai-at-N4eW8tOk3nV6yU2c".into());
        b.tool = Some(ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "echo xai-rt-H3nJ6sD1fG5kL0pX"}),
            summary: "echo xai-rt-H3nJ6sD1fG5kL0pX".into(),
        });
        b.result = Some(ToolResult {
            tool_id: "t1".into(),
            output: r#"{"access_token": "xai-at-N4eW8tOk3nV6yU2c"}"#.into(),
            truncated: false,
            is_error: false,
        });
        redact_block(&r, &View::All, &mut b);
        let text = serde_json::to_string(&b).unwrap();
        assert!(
            !text.contains("xai-at-") && !text.contains("xai-rt-"),
            "{text}"
        );
        // An event payload or an audit detail (`Tx::emit`, `Tx::audit`).
        let v = crate::db::redact_json_values(
            &r,
            &View::All,
            serde_json::json!({"error": "grok: refresh failed for xai-rt-H3nJ6sD1fG5kL0pX",
                               "lines": ["xai-at-N4eW8tOk3nV6yU2c"]}),
        );
        let text = v.to_string();
        assert!(
            !text.contains("xai-at-") && !text.contains("xai-rt-"),
            "{text}"
        );
        // The logout removes the account. Its tokens stay in the matcher
        // for the life of the daemon (TER-1257); no Grok file is read.
        assert!(crate::accounts::remove(dir.path(), "x-super").unwrap());
        assert!(crate::accounts::grok_secret_values(dir.path()).is_empty());
        let r = build(&c, dir.path()).unwrap();
        assert_eq!(
            r.redact_str("xai-at-N4eW8tOk3nV6yU2c xai-rt-H3nJ6sD1fG5kL0pX"),
            "[secret:subscription/x-super] [secret:subscription/x-super]"
        );
    }

    /// TER-630: the matcher has the AWS access key ID of a Bedrock key, and
    /// the `private_key` and `private_key_id` of a GCP service-account JSON,
    /// beside the keys.
    #[test]
    fn cloud_key_parts_are_in_the_matcher() {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::secrets::tests::db();
        let k = MasterKey::generate();
        crypto::write_key(&crypto::key_path(dir.path()), &k).unwrap();
        let key = |provider: &str, key: &str, key_id: Option<&str>| crate::login_session::ApiKey {
            provider: provider.into(),
            key: key.into(),
            plan: None,
            base_url: None,
            models: Vec::new(),
            key_id: key_id.map(str::to_owned),
            cloud: None,
        };
        crate::login_session::store_key(
            dir.path(),
            "aws",
            &key(
                "bedrock",
                "aws-secret-access-key-abc",
                Some("AKIAEXAMPLEKEYID"),
            ),
        )
        .unwrap();
        let json = r#"{"type":"service_account","private_key_id":"gcpkeyid0123456789","private_key":"-----BEGIN PRIVATE KEY-----MIIEgcpsecretbody-----END PRIVATE KEY-----"}"#;
        crate::login_session::store_key(dir.path(), "vertex", &key("gcp", json, None)).unwrap();

        let r = build(&c, dir.path()).unwrap();
        assert_eq!(
            r.redact_str("id AKIAEXAMPLEKEYID secret aws-secret-access-key-abc"),
            "id [secret:subscription/aws] secret [secret:subscription/aws]"
        );
        assert_eq!(
            r.redact_str(
                "kid gcpkeyid0123456789 pk -----BEGIN PRIVATE KEY-----MIIEgcpsecretbody-----END PRIVATE KEY-----"
            ),
            "kid [secret:subscription/vertex] pk [secret:subscription/vertex]"
        );
    }

    #[test]
    fn no_secret_is_the_empty_matcher_without_a_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::secrets::tests::db();
        assert!(build(&c, dir.path()).unwrap().is_empty());
    }

    fn put_kind(
        c: &Connection,
        k: &MasterKey,
        scope: &str,
        name: &str,
        kind: Kind,
        fields: serde_json::Value,
        strict: bool,
    ) -> ApiResult<crate::secrets::SecretInfo> {
        let input: SecretInput = serde_json::from_value(fields).unwrap();
        put(
            c,
            k,
            PutMode::Upsert,
            &NewSecret {
                scope,
                name,
                kind,
                input: &input,
                by: "operator",
                strict,
            },
        )
        .map(|(i, _)| i)
    }

    /// S4 (b): a put refuses a value that looks like an identifier, and a
    /// weak token; a weak login password is stored with a warning.
    #[test]
    fn a_put_refuses_identifier_and_weak_values() {
        let c = crate::secrets::grants::tests::db();
        let k = MasterKey::generate();
        let token = |v: &str| {
            put_kind(
                &c,
                &k,
                "p",
                "t",
                Kind::Token,
                serde_json::json!({"fields": {"value": v}}),
                true,
            )
        };
        // The ULID of an agent, an actor label, record words, a name here.
        for v in [
            crate::secrets::grants::tests::AGENT,
            "agent:01K8AGENT0000000000000000A",
            "operator",
            "grantee_label",
        ] {
            let e = token(v).unwrap_err();
            assert_eq!(e.code, crate::error::Code::Invalid, "{v}");
            assert!(
                e.message.contains("cannot be a secret"),
                "{v}: {}",
                e.message
            );
        }
        // A long name of this instance.
        c.execute(
            "INSERT INTO projects (name, created_at, updated_at)
             VALUES ('a-long-project-name', 'x', 'x')",
            [],
        )
        .unwrap();
        let e = token("a-long-project-name").unwrap_err();
        assert!(e.message.contains("name of a project"), "{}", e.message);
        // A weak token.
        let e = token("hostname1").unwrap_err();
        assert!(e.message.contains("is weak"), "{}", e.message);
        // A real token.
        let info = token("ghp_R3alLook1ngT0ken").unwrap();
        assert!(info.warnings.is_empty());
        // A weak login password: stored, with a warning.
        let info = put_kind(
            &c,
            &k,
            "p",
            "win",
            Kind::Login,
            serde_json::json!({"fields": {"username": "bob", "password": "summer2024"}}),
            true,
        )
        .unwrap();
        assert!(info.has_value);
        assert_eq!(info.warnings.len(), 1, "{:?}", info.warnings);
        assert!(info.warnings[0].contains("redaction does not hide it"));
        // An identifier login password is refused all the same.
        let e = put_kind(
            &c,
            &k,
            "p",
            "win",
            Kind::Login,
            serde_json::json!({"fields": {"password": "Operator"}}),
            true,
        )
        .unwrap_err();
        assert!(e.message.contains("cannot be a secret"), "{}", e.message);
        // A real ssh key passes, line by line.
        put_kind(
            &c,
            &k,
            "p",
            "key",
            Kind::SshKey,
            serde_json::json!({"fields": {"private_key": crate::secrets::ssh::tests::ed25519_key()}}),
            true,
        )
        .unwrap();
    }

    /// S4: the matcher leaves out a weak value and an identifier value
    /// that a put did not check (the migration of old files, or a store
    /// from before the check), and a value equal to a name made later.
    #[test]
    fn the_matcher_leaves_out_weak_and_identifier_values() {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::secrets::grants::tests::db();
        let k = MasterKey::generate();
        crypto::write_key(&crypto::key_path(dir.path()), &k).unwrap();
        for (name, value) in [
            ("ulid", crate::secrets::grants::tests::AGENT),
            ("word", "operator"),
            ("weak", "hostname1"),
            ("later", "a-name-made-later-1"),
            ("good", "ghp_R3alLook1ngT0ken"),
        ] {
            put_kind(
                &c,
                &k,
                "p",
                name,
                Kind::Token,
                serde_json::json!({"fields": {"value": value}}),
                false,
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO projects (name, created_at, updated_at)
             VALUES ('a-name-made-later-1', 'x', 'x')",
            [],
        )
        .unwrap();
        let r = build(&c, dir.path()).unwrap();
        let text = format!(
            "{} operator hostname1 a-name-made-later-1 ghp_R3alLook1ngT0ken",
            crate::secrets::grants::tests::AGENT
        );
        assert_eq!(
            r.redact_str(&text),
            format!(
                "{} operator hostname1 a-name-made-later-1 [secret:p/good]",
                crate::secrets::grants::tests::AGENT
            )
        );
    }

    #[test]
    fn the_view_of_an_actor() {
        assert_eq!(view_of(&Actor::Operator), View::All);
    }
}
