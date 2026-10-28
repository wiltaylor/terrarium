//! The ssh-agent broker of one agent (PLAN §16b.4): a [`KeySource`] of the
//! agent's granted `ssh_key` secrets, and the socket that serves it.
//!
//! The source reads the grants at each list and each sign, so a revoke
//! applies at once. A sign opens the secret and makes the signing key (a
//! key with a passphrase runs its KDF, bcrypt), signs, and keeps the key
//! for [`KEY_CACHE_TTL`] in a cache of this agent's source only. The next
//! signs in that time use the cached key: a loop of signs does not run the
//! KDF again and again. A sign checks the grant first; with no grant it
//! removes the key from the cache. Each request removes the expired keys,
//! and a stop of the socket drops the source. The key types zeroize on
//! drop. The socket allows [`SIGN_LIMIT`] signs (a token bucket); a sign
//! over it fails. Each sign counts in the audit: one `secret.use` row each
//! minute for each key and agent, with the number of good signs (`signs`)
//! and of the signs over the limit (`limited`) in that minute.
//!
//! | Path | Mode | Holds |
//! |---|---|---|
//! | `<runtime>/ssh-agent/` | 0700 | one dir for each agent with an ssh_key grant |
//! | `<runtime>/ssh-agent/<ulid>/` | 0700 | bound read-only into the sandbox at [`SANDBOX_DIR`] |
//! | `<runtime>/ssh-agent/<ulid>/agent.sock` | 0600 | the socket |
//!
//! The sandbox binds the directory, not the socket file: a daemon that
//! starts again makes a new socket file (a new inode) in the same
//! directory, and an agent that kept running sees it.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ssh_key::private::{EcdsaKeypair, Ed25519Keypair, RsaKeypair};
use ssh_key::{PrivateKey, PublicKey, Signature};

use super::grants;
use crate::db::Db;
use crate::ssh_agent::{self, AgentHandle, AgentOptions, Identity, KeySource, SignError};

/// The directory of the socket inside the sandbox.
pub const SANDBOX_DIR: &str = "/run/terrarium/ssh-agent";
/// `SSH_AUTH_SOCK` inside the sandbox: a symlink to the socket in
/// [`SANDBOX_DIR`].
pub const SANDBOX_SOCK: &str = "/run/terrarium/ssh-agent.sock";
/// The file name of the socket in its directory.
pub const SOCK_NAME: &str = "agent.sock";
/// One audit row for each key and agent in this time.
const AUDIT_WINDOW: Duration = Duration::from_secs(60);
/// How long the source keeps an opened signing key.
pub const KEY_CACHE_TTL: Duration = Duration::from_secs(60);
/// The sign limit of each agent socket: 10 a second, 30 at once.
pub const SIGN_LIMIT: ssh_agent::SignLimit = ssh_agent::SignLimit {
    per_second: 10,
    burst: 30,
};

/// `<runtime>/ssh-agent/<ulid>`: the socket directory of an agent.
pub fn socket_dir(runtime_dir: &Path, agent_id: &str) -> PathBuf {
    runtime_dir.join("ssh-agent").join(agent_id)
}

/// The signing key of a stored `ssh_key`: the OpenSSH format (opened with
/// the passphrase when it has one), or PEM with no passphrase (PKCS#1 RSA,
/// PKCS#8 RSA, Ed25519, or P-256, SEC1 P-256).
pub fn signing_key(pem: &str, passphrase: Option<&str>) -> Result<PrivateKey, String> {
    use ed25519_dalek::pkcs8::DecodePrivateKey as _;
    use rsa::pkcs1::DecodeRsaPrivateKey as _;
    let s = pem.trim();
    if s.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----") {
        let key =
            PrivateKey::from_openssh(s).map_err(|e| format!("the key does not parse: {e}"))?;
        return match (key.is_encrypted(), passphrase) {
            (false, _) => Ok(key),
            (true, Some(p)) => key
                .decrypt(p)
                .map_err(|_| "the passphrase does not open the key".to_owned()),
            (true, None) => Err("the key has a passphrase, and the secret has none".into()),
        };
    }
    let rsa = |k: rsa::RsaPrivateKey| -> Result<PrivateKey, String> {
        RsaKeypair::try_from(k)
            .map(PrivateKey::from)
            .map_err(|e| format!("the RSA key is not usable: {e}"))
    };
    let p256 = |k: p256::SecretKey| -> PrivateKey {
        let public = k.public_key().into();
        PrivateKey::from(EcdsaKeypair::NistP256 {
            public,
            private: k.into(),
        })
    };
    if s.starts_with("-----BEGIN RSA PRIVATE KEY-----") {
        let k = rsa::RsaPrivateKey::from_pkcs1_pem(s).map_err(|_| "the RSA key does not parse")?;
        return rsa(k);
    }
    if s.starts_with("-----BEGIN EC PRIVATE KEY-----") {
        let k = p256::SecretKey::from_sec1_pem(s).map_err(|_| "the EC key does not parse")?;
        return Ok(p256(k));
    }
    if s.starts_with("-----BEGIN PRIVATE KEY-----") {
        if let Ok(k) = rsa::RsaPrivateKey::from_pkcs8_pem(s) {
            return rsa(k);
        }
        if let Ok(k) = ed25519_dalek::SigningKey::from_pkcs8_pem(s) {
            return Ok(PrivateKey::from(Ed25519Keypair::from(&k)));
        }
        if let Ok(k) = p256::SecretKey::from_pkcs8_pem(s) {
            return Ok(p256(k));
        }
    }
    Err("the key is not in a format that the ssh-agent signs with".into())
}

/// An opened signing key in the cache of [`GrantedKeys`].
struct CachedKey {
    key: Arc<PrivateKey>,
    at: Instant,
}

/// The granted `ssh_key` secrets of one agent.
pub struct GrantedKeys {
    db: Db,
    state_dir: PathBuf,
    agent_id: String,
    project: String,
    /// The opened keys by label, for [`KEY_CACHE_TTL`].
    cache: Mutex<HashMap<String, CachedKey>>,
    ttl: Duration,
    /// The count of opens (each one a decrypt and, with a passphrase, a
    /// KDF run).
    opens: AtomicU64,
}

impl GrantedKeys {
    pub fn new(db: Db, state_dir: PathBuf, agent_id: String, project: String) -> Self {
        Self {
            db,
            state_dir,
            agent_id,
            project,
            cache: Mutex::default(),
            ttl: KEY_CACHE_TTL,
            opens: AtomicU64::new(0),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, CachedKey>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Remove the expired keys (the drop zeroizes them).
    fn expire(&self) {
        let ttl = self.ttl;
        self.cache().retain(|_, c| c.at.elapsed() < ttl);
    }

    /// The cached key of `identity`, when it is young and still has the
    /// listed public key.
    fn cached(&self, identity: &Identity) -> Option<Arc<PrivateKey>> {
        self.expire();
        let mut cache = self.cache();
        let hit = cache
            .get(&identity.label)
            .filter(|c| c.key.public_key().key_data() == identity.public.key_data())
            .map(|c| c.key.clone());
        if hit.is_none() {
            cache.remove(&identity.label);
        }
        hit
    }

    /// Open the stored key of `e` (a decrypt, and a KDF run for a key with
    /// a passphrase).
    fn open_key(&self, e: &grants::Effective) -> Result<PrivateKey, SignError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let opened = self
            .db
            .read_now(|c| {
                let key = super::key(&self.state_dir)?;
                super::open(c, &key, &e.scope, &e.name)
            })
            .map_err(|err| SignError::Failed(err.message))?
            .ok_or_else(|| SignError::Refused("the secret is gone".into()))?;
        let pem = opened
            .values
            .get("private_key")
            .ok_or_else(|| SignError::Failed("the secret has no private_key".into()))?;
        signing_key(pem, opened.values.get("passphrase").map(|p| p.as_str()))
            .map_err(SignError::Failed)
    }

    fn granted(&self) -> Vec<grants::Effective> {
        self.db
            .read_now(|c| grants::ssh_keys(c, &self.agent_id, &self.project))
            .unwrap_or_else(|e| {
                tracing::warn!(agent = %self.agent_id, "ssh-agent: cannot read the grants: {e}");
                Vec::new()
            })
    }
}

/// The label of a key: `<scope>/<name>`.
fn label(e: &grants::Effective) -> String {
    format!("{}/{}", e.scope, e.name)
}

impl KeySource for GrantedKeys {
    fn list(&self) -> Vec<Identity> {
        self.expire();
        self.granted()
            .into_iter()
            .filter_map(|e| {
                let text = e.fields.get("public_key").and_then(|v| v.as_str());
                let public = text.and_then(|t| PublicKey::from_openssh(t).ok());
                if public.is_none() {
                    tracing::warn!(
                        agent = %self.agent_id, key = %label(&e),
                        "ssh-agent: the secret has no public key; store the key again to use it"
                    );
                }
                Some(Identity {
                    label: label(&e),
                    public: public?,
                })
            })
            .collect()
    }

    fn sign(&self, identity: &Identity, data: &[u8], flags: u32) -> Result<Signature, SignError> {
        // The grant first, at each sign: a revoke applies at once, and it
        // removes the cached key.
        let Some(e) = self
            .granted()
            .into_iter()
            .find(|e| label(e) == identity.label)
        else {
            self.cache().remove(&identity.label);
            return Err(SignError::Refused("the grant is gone".into()));
        };
        if let Some(key) = self.cached(identity) {
            return ssh_agent::sign_with_key(&key, data, flags);
        }
        let key = self.open_key(&e)?;
        if key.public_key().key_data() != identity.public.key_data() {
            return Err(SignError::Refused(
                "the stored key changed since the list".into(),
            ));
        }
        let key = Arc::new(key);
        let signed = ssh_agent::sign_with_key(&key, data, flags);
        self.cache().insert(
            identity.label.clone(),
            CachedKey {
                key,
                at: Instant::now(),
            },
        );
        signed
    }
}

/// The audit row of one key in the current minute. `signs` is shared
/// with the write tasks: each task writes the count at its own time, so
/// the tasks can run in any order.
struct Window {
    id: String,
    since: Instant,
    signs: Arc<AtomicU64>,
    /// The signs over [`SIGN_LIMIT`].
    limited: Arc<AtomicU64>,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            since: now,
            signs: Arc::default(),
            limited: Arc::default(),
        }
    }
}

/// The `on_sign` hook: one `secret.use` row each minute for each key, with
/// the count of signs. It runs in the runtime of the server.
fn audit_hook(db: Db, agent_id: String) -> ssh_agent::SignHook {
    let windows: Arc<Mutex<HashMap<String, Window>>> = Arc::default();
    Arc::new(move |ev: &ssh_agent::SignEvent| {
        let limited = ev.outcome == ssh_agent::SignOutcome::Limited;
        if !ev.ok() && !limited {
            return;
        }
        let now = Instant::now();
        let (id, signs, over) = {
            let mut w = windows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let win = w
                .entry(ev.label.clone())
                .and_modify(|win| {
                    if now.duration_since(win.since) >= AUDIT_WINDOW {
                        *win = Window::new(now);
                    }
                })
                .or_insert_with(|| Window::new(now));
            if limited {
                win.limited.fetch_add(1, Ordering::SeqCst);
            } else {
                win.signs.fetch_add(1, Ordering::SeqCst);
            }
            (win.id.clone(), win.signs.clone(), win.limited.clone())
        };
        let (db, agent, key) = (db.clone(), agent_id.clone(), ev.label.clone());
        tokio::spawn(async move {
            let r = db
                .write(format!("agent:{agent}"), move |t| {
                    let (scope, name) = key.split_once('/').unwrap_or(("", key.as_str()));
                    let detail = serde_json::json!({
                        "agent": agent, "scope": scope, "name": name,
                        "broker": "ssh-agent", "signs": signs.load(Ordering::SeqCst),
                        "limited": over.load(Ordering::SeqCst),
                    });
                    let n: i64 =
                        t.query_row("SELECT count(*) FROM audit WHERE id = ?1", [&id], |r| {
                            r.get(0)
                        })?;
                    if n == 0 {
                        let project = super::scope_project(scope);
                        t.audit_with_id(&id, "secret.use", &key, project, detail)
                    } else {
                        t.audit_update(&id, detail)
                    }
                })
                .await;
            if let Err(e) = r {
                tracing::warn!("ssh-agent: cannot write the audit: {e}");
            }
        });
    })
}

/// Serve the ssh-agent socket of one agent at
/// `<runtime>/ssh-agent/<ulid>/agent.sock` (the dirs 0700, the socket
/// 0600). Call it in a Tokio runtime. A drop of the handle stops it.
pub fn serve(
    db: Db,
    state_dir: PathBuf,
    runtime_dir: &Path,
    agent_id: &str,
    project: &str,
) -> io::Result<AgentHandle> {
    let dir = socket_dir(runtime_dir, agent_id);
    for d in [dir.parent().unwrap_or(&dir), dir.as_path()] {
        std::fs::create_dir_all(d)?;
        // No symlink follow: the mode goes on a real dir only.
        let meta = std::fs::symlink_metadata(d)?;
        if !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", d.display()),
            ));
        }
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
    }
    let source = GrantedKeys::new(
        db.clone(),
        state_dir,
        agent_id.to_owned(),
        project.to_owned(),
    );
    let opts = AgentOptions {
        on_sign: Some(audit_hook(db, agent_id.to_owned())),
        sign_limit: Some(SIGN_LIMIT),
        ..AgentOptions::default()
    };
    ssh_agent::serve(dir.join(SOCK_NAME), Arc::new(source), opts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use crate::secrets::{GLOBAL, Kind, MasterKey};
    use serde_json::json;
    use signature::Verifier as _;

    pub(crate) async fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::secrets::grants::tests::db();
        crate::secrets::ensure_key(&c, dir.path()).unwrap();
        let k: MasterKey = crate::secrets::key(dir.path()).unwrap();
        let pem = crate::secrets::ssh::tests::ed25519_key();
        let s = crate::secrets::grants::tests::secret(
            &c,
            &k,
            GLOBAL,
            "deploy",
            Kind::SshKey,
            json!({"fields": {"private_key": pem.as_str()}}),
        );
        crate::secrets::grants::tests::grant(&c, &s, "agent:p/a", None, None).unwrap();
        let public = s.fields["public_key"].as_str().unwrap().to_owned();
        let db = Db::new(c, crate::events::EventHub::default());
        (dir, db, public)
    }

    #[tokio::test]
    async fn the_source_lists_and_signs_granted_keys_only() {
        let (dir, db, public) = fixture().await;
        let a = crate::secrets::grants::tests::AGENT;
        let b = crate::secrets::grants::tests::OTHER;
        let mine = GrantedKeys::new(db.clone(), dir.path().into(), a.into(), "p".into());
        let other = GrantedKeys::new(db.clone(), dir.path().into(), b.into(), "q".into());
        let ids = mine.list();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].label, "global/deploy");
        assert_eq!(ids[0].public.to_openssh().unwrap(), public.trim());
        assert!(other.list().is_empty());
        let sig = mine.sign(&ids[0], b"data", 0).unwrap();
        ids[0].public.key_data().verify(b"data", &sig).unwrap();
        assert!(matches!(
            other.sign(&ids[0], b"data", 0),
            Err(SignError::Refused(_))
        ));
        // A revoke applies at once.
        db.read_now(|c| {
            let s = crate::secrets::get(c, GLOBAL, "deploy")?.unwrap();
            crate::secrets::grants::delete(c, &s, &format!("agent:{a}"))
        })
        .unwrap();
        assert!(mine.list().is_empty());
        assert!(matches!(
            mine.sign(&ids[0], b"data", 0),
            Err(SignError::Refused(_))
        ));
    }

    /// S10 (secret manager review, 2026-10-29): a loop of signs opens the
    /// key once, not at each sign; a revoke empties the cache at once.
    #[tokio::test]
    async fn signs_open_the_key_once_in_the_cache_time() {
        let (dir, db, _) = fixture().await;
        let a = crate::secrets::grants::tests::AGENT;
        let mine = GrantedKeys::new(db.clone(), dir.path().into(), a.into(), "p".into());
        let ids = mine.list();
        for i in 0..5u8 {
            let sig = mine.sign(&ids[0], &[i], 0).unwrap();
            ids[0].public.key_data().verify(&[i], &sig).unwrap();
        }
        assert_eq!(mine.opens.load(Ordering::SeqCst), 1);
        assert_eq!(mine.cache().len(), 1);
        db.read_now(|c| {
            let s = crate::secrets::get(c, GLOBAL, "deploy")?.unwrap();
            crate::secrets::grants::delete(c, &s, &format!("agent:{a}"))
        })
        .unwrap();
        assert!(matches!(
            mine.sign(&ids[0], b"x", 0),
            Err(SignError::Refused(_))
        ));
        assert!(mine.cache().is_empty());
        // An expired key is opened again.
        let mut short = GrantedKeys::new(db.clone(), dir.path().into(), a.into(), "p".into());
        short.ttl = Duration::ZERO;
        db.read_now(|c| {
            let s = crate::secrets::get(c, GLOBAL, "deploy")?.unwrap();
            crate::secrets::grants::tests::grant(c, &s, "agent:p/a", None, None).map(|_| ())
        })
        .unwrap();
        let ids = short.list();
        short.sign(&ids[0], b"1", 0).unwrap();
        short.sign(&ids[0], b"2", 0).unwrap();
        assert_eq!(short.opens.load(Ordering::SeqCst), 2);
        short.list();
        assert!(short.cache().is_empty());
    }

    #[tokio::test]
    async fn the_socket_serves_the_grants_and_audits_once_a_minute() {
        let (dir, db, _) = fixture().await;
        let a = crate::secrets::grants::tests::AGENT;
        let run = dir.path().join("run");
        let h = serve(db.clone(), dir.path().into(), &run, a, "p").unwrap();
        assert_eq!(h.path(), socket_dir(&run, a).join(SOCK_NAME));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(h.path()), 0o600);
        assert_eq!(mode(&socket_dir(&run, a)), 0o700);
        let pk = GrantedKeys::new(db.clone(), dir.path().into(), a.into(), "p".into()).list();
        let hook = audit_hook(db.clone(), a.into());
        for _ in 0..3 {
            hook(&ssh_agent::SignEvent {
                label: pk[0].label.clone(),
                outcome: ssh_agent::SignOutcome::Signed,
            });
        }
        for _ in 0..2 {
            hook(&ssh_agent::SignEvent {
                label: pk[0].label.clone(),
                outcome: ssh_agent::SignOutcome::Limited,
            });
        }
        // The audit writes run as tasks.
        let mut rows = Vec::new();
        for _ in 0..100 {
            rows = db
                .read(|c| {
                    let mut st = c.prepare(
                        "SELECT detail FROM audit WHERE action = 'secret.use' ORDER BY at",
                    )?;
                    let r = st
                        .query_map([], |r| r.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(r)
                })
                .await
                .unwrap();
            if rows.len() == 1
                && rows[0].contains("\"signs\":3")
                && rows[0].contains("\"limited\":2")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains("\"signs\":3"), "{rows:?}");
        assert!(rows[0].contains("\"limited\":2"), "{rows:?}");
        assert!(!rows[0].contains("PRIVATE"));
        h.shutdown().await;
        assert!(!socket_dir(&run, a).join(SOCK_NAME).exists());
    }
}
