//! `terrarium login`, `terrarium logout`, and `terrarium admin password
//! set` (`docs/cli.md`, `docs/api.md` §1.1).
//!
//! `login` trades the operator password for a CLI session token
//! (`POST /api/session` with `{"password", "client": "cli"}`) and keeps it
//! for the daemon URL (`crate::credentials`). Later commands send it as
//! `Authorization: Bearer` when `TERRARIUM_TOKEN` is not set.

use std::sync::Arc;

use anyhow::Context;
use serde_json::{Value, json};

use crate::api::{Api, CliError};
use crate::credentials::{Entry, Store, TokenSource};
use crate::ctx::Ctx;
use crate::transport::{Endpoint, HttpTransport};

fn no_agents(ctx: &Ctx) -> anyhow::Result<()> {
    if ctx.is_agent() {
        return Err(CliError::usage(
            "an agent does not log in: it talks to the daemon through its runner socket",
        ));
    }
    Ok(())
}

/// The daemon URL, its key in the store, and the store.
fn target(ctx: &Ctx) -> anyhow::Result<(Endpoint, String, Store)> {
    let endpoint = ctx
        .http
        .as_ref()
        .map(|h| h.endpoint.clone())
        .ok_or_else(|| CliError::usage("no daemon URL"))?;
    let key = crate::credentials::key(&endpoint).ok_or_else(|| {
        CliError::usage(
            "login is for a TCP daemon: name it with --url http://host:8080 or TERRARIUM_URL \
             (the Unix socket needs no login)",
        )
    })?;
    let store = ctx
        .credentials
        .clone()
        .ok_or_else(|| CliError::usage("no config dir: set XDG_CONFIG_HOME or HOME"))?;
    Ok((endpoint, key, store))
}

/// Ask on the terminal, with no echo.
fn prompt(label: &str) -> anyhow::Result<String> {
    rpassword::prompt_password(label)
        .context("cannot read the password from the terminal (use the stdin flag in a script)")
}

/// The lines of stdin (or of the test stdin), with no line ends.
fn stdin_lines(ctx: &Ctx) -> anyhow::Result<Vec<String>> {
    Ok(ctx
        .text("-")?
        .lines()
        .map(|l| l.trim_end_matches('\r').to_owned())
        .collect())
}

/// `POST /api/session` with the password, and keep the token for `key`.
pub async fn login_with(
    api: &Api,
    store: &Store,
    key: &str,
    password: &str,
) -> anyhow::Result<Entry> {
    let v = api
        .post(
            "/api/session",
            Some(json!({ "password": password, "client": "cli" })),
        )
        .await?;
    let token = v["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("the daemon answered no session token; is it older than this CLI?")
        })?;
    let entry = Entry {
        token: token.to_owned(),
        expires_at: v["expires_at"].as_str().map(str::to_owned),
    };
    store.put(key, entry.clone())?;
    Ok(entry)
}

/// The refusal of a plain `http://` login to another host.
const CLEAR_TEXT: &str = "this login would send the operator password, and then a 90-day session \
     token, in clear text over http:// to another host. Use https:// (a TLS proxy in front of the \
     daemon), or log in on the daemon host. On a network that encrypts by itself (a Tailscale \
     tailnet, a WireGuard or SSH tunnel), add --insecure-http to accept the risk";

/// May `login` send the password to `endpoint`? A TLS or loopback URL:
/// yes. A plain URL to another host: only with `--insecure-http`, and with
/// a loud warning.
pub fn check_transport(endpoint: &Endpoint, insecure_http: bool) -> anyhow::Result<bool> {
    if endpoint.is_private() {
        return Ok(false);
    }
    if !insecure_http {
        return Err(CliError::usage(CLEAR_TEXT));
    }
    Ok(true)
}

/// `terrarium login [--password-stdin] [--insecure-http]`.
pub async fn login(ctx: &Ctx, password_stdin: bool, insecure_http: bool) -> anyhow::Result<()> {
    no_agents(ctx)?;
    let (endpoint, key, store) = target(ctx)?;
    if check_transport(&endpoint, insecure_http)? {
        eprintln!(
            "terrarium: WARNING: --insecure-http: the password and the session token cross the \
             network in clear text to {key}. Anyone on the path can read them. This is safe only \
             when the network itself encrypts (a Tailscale tailnet is WireGuard, so plain HTTP \
             inside it is encrypted)."
        );
    }
    let password = if password_stdin {
        stdin_lines(ctx)?.into_iter().next().unwrap_or_default()
    } else {
        prompt(&format!("Operator password for {key}: "))?
    };
    if password.is_empty() {
        return Err(CliError::usage("no password given"));
    }
    // The login sends no token: not TERRARIUM_TOKEN, not an old session.
    let api = Api::new(Arc::new(HttpTransport::new(endpoint, None)));
    let entry = login_with(&api, &store, &key, &password).await?;
    let v = json!({ "url": key, "expires_at": entry.expires_at, "file": store.path() });
    ctx.emit(&v, |c, v| {
        c.line(format!(
            "logged in to {}; the session ends {}",
            v["url"].as_str().unwrap_or_default(),
            v["expires_at"].as_str().unwrap_or("(not known)")
        ));
    })
}

/// `DELETE /api/session` with the stored token, then forget it. The local
/// copy goes even when the daemon cannot be reached.
pub async fn logout_with(api: &Api, store: &Store, key: &str) -> anyhow::Result<Option<String>> {
    let warning = api
        .delete("/api/session", Vec::new())
        .await
        .err()
        .map(|e| format!("{e:#}"));
    store.remove(key)?;
    Ok(warning)
}

/// `terrarium logout`.
pub async fn logout(ctx: &Ctx) -> anyhow::Result<()> {
    no_agents(ctx)?;
    let (endpoint, key, store) = target(ctx)?;
    let Some(entry) = store.get(&key)? else {
        ctx.line(format!("not logged in to {key}"));
        return Ok(());
    };
    let api = Api::new(Arc::new(HttpTransport::new(endpoint, Some(entry.token))));
    if let Some(w) = logout_with(&api, &store, &key).await? {
        eprintln!("terrarium: the daemon did not end the session ({w}); the local copy is gone");
    }
    ctx.line(format!("logged out of {key}"));
    Ok(())
}

/// `terrarium admin password set [--stdin]`. With a stored session the
/// daemon wants the current password too. The change ends every session,
/// so the stored one goes.
pub async fn password_set(ctx: &Ctx, stdin: bool) -> anyhow::Result<()> {
    let session = ctx.token_source == TokenSource::Stored;
    let (current, new) = if stdin {
        let mut lines = stdin_lines(ctx)?.into_iter();
        let current = if session { lines.next() } else { None };
        (current, lines.next().unwrap_or_default())
    } else {
        let current = if session {
            Some(prompt("Current password: ")?)
        } else {
            None
        };
        let new = prompt("New password: ")?;
        if prompt("Again: ")? != new {
            return Err(CliError::usage("the two passwords are not the same"));
        }
        (current, new)
    };
    if new.is_empty() {
        return Err(CliError::usage("no new password given"));
    }
    let mut body = json!({ "new": new });
    if let Some(c) = current {
        body["current"] = Value::String(c);
    }
    ctx.api.put("/api/auth/password", body).await?;
    if session && let Ok((_, key, store)) = target(ctx) {
        store.remove(&key)?;
        ctx.line("password changed; every session has ended: run terrarium login");
    } else {
        ctx.line("password changed; every session has ended");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::transport::Method;
    use crate::transport::mock::MockTransport;

    const URL: &str = "http://terrarium.lan:8080";

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::new(tmp.path().join("terrarium"));
        (tmp, s)
    }

    #[tokio::test]
    async fn login_keeps_the_token() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, s) = store();
        let mock = MockTransport::new();
        mock.reply(
            201,
            json!({"kind": "cli", "token": "eyJ.x.y", "exp": 1, "expires_at": "2027-01-25T10:00:00Z"}),
        );
        let api = Api::new(Arc::new(mock.clone()));
        let e = login_with(&api, &s, URL, "the password").await.unwrap();
        assert_eq!(e.token, "eyJ.x.y");
        let req = &mock.requests()[0];
        assert_eq!(
            (req.method, req.path.as_str()),
            (Method::Post, "/api/session")
        );
        assert_eq!(
            req.body,
            Some(json!({"password": "the password", "client": "cli"}))
        );
        assert_eq!(s.get(URL).unwrap(), Some(e));
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let text = std::fs::read_to_string(s.path()).unwrap();
        assert!(!text.contains("the password"));
    }

    #[tokio::test]
    async fn login_failure_stores_nothing() {
        let (_tmp, s) = store();
        let mock = MockTransport::new();
        mock.reply(
            401,
            json!({"error": {"code": "unauthorized", "message": "login failed"}}),
        );
        let api = Api::new(Arc::new(mock));
        let err = login_with(&api, &s, URL, "wrong").await.unwrap_err();
        assert!(err.to_string().contains("login failed"));
        assert!(s.load().unwrap().is_empty());
    }

    #[tokio::test]
    async fn logout_ends_and_forgets() {
        let (_tmp, s) = store();
        s.put(
            URL,
            Entry {
                token: "t".into(),
                expires_at: None,
            },
        )
        .unwrap();
        let mock = MockTransport::new();
        mock.reply(204, Value::Null);
        let api = Api::new(Arc::new(mock.clone()));
        assert_eq!(logout_with(&api, &s, URL).await.unwrap(), None);
        assert_eq!(mock.requests()[0].method, Method::Delete);
        assert_eq!(mock.requests()[0].path, "/api/session");
        assert_eq!(s.get(URL).unwrap(), None);
        // A daemon error still forgets the local copy.
        s.put(
            URL,
            Entry {
                token: "t".into(),
                expires_at: None,
            },
        )
        .unwrap();
        let mock = MockTransport::new();
        mock.reply(
            500,
            json!({"error": {"code": "internal", "message": "down"}}),
        );
        let api = Api::new(Arc::new(mock));
        assert!(logout_with(&api, &s, URL).await.unwrap().is_some());
        assert_eq!(s.get(URL).unwrap(), None);
    }

    fn ctx(mock: &MockTransport, s: &Store, source: TokenSource, stdin: &str) -> Ctx {
        let http = HttpTransport::new(Endpoint::from_url(URL).unwrap(), None);
        let mut c = Ctx::for_test(Arc::new(mock.clone()), None).with_stdin(stdin);
        c.http = Some(http);
        c.credentials = Some(s.clone());
        c.token_source = source;
        c
    }

    #[tokio::test]
    async fn password_set_with_the_static_token() {
        let (_tmp, s) = store();
        let mock = MockTransport::new();
        mock.reply(204, Value::Null);
        let c = ctx(&mock, &s, TokenSource::Env, "a new password!\n");
        password_set(&c, true).await.unwrap();
        let req = &mock.requests()[0];
        assert_eq!(
            (req.method, req.path.as_str()),
            (Method::Put, "/api/auth/password")
        );
        assert_eq!(req.body, Some(json!({"new": "a new password!"})));
    }

    /// With a stored session: the current password goes too, and the
    /// stored session goes after the change.
    #[tokio::test]
    async fn password_set_with_a_stored_session() {
        let (_tmp, s) = store();
        s.put(
            URL,
            Entry {
                token: "t".into(),
                expires_at: None,
            },
        )
        .unwrap();
        let mock = MockTransport::new();
        mock.reply(204, Value::Null);
        let c = ctx(
            &mock,
            &s,
            TokenSource::Stored,
            "old password\nnew password!\n",
        );
        password_set(&c, true).await.unwrap();
        assert_eq!(
            mock.requests()[0].body,
            Some(json!({"current": "old password", "new": "new password!"}))
        );
        assert_eq!(s.get(URL).unwrap(), None);
        assert!(c.out.captured().contains("terrarium login"));
    }

    #[tokio::test]
    async fn agents_do_not_log_in() {
        let (_tmp, s) = store();
        let mock = MockTransport::new();
        let mut c = ctx(&mock, &s, TokenSource::None, "pw\n");
        c.agent = Some(crate::ctx::AgentEnv {
            agent: "A".into(),
            socket: "/nonexistent.sock".into(),
        });
        assert!(login(&c, true, false).await.is_err());
        assert!(logout(&c).await.is_err());
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn plain_http_login_needs_the_flag() {
        let ep = |u: &str| Endpoint::from_url(u).unwrap();
        for u in [
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "https://t.example.com",
        ] {
            assert!(!check_transport(&ep(u), false).unwrap(), "{u}");
        }
        for u in [
            "http://terrarium.lan:8080",
            "http://box.tail1234.ts.net:8080",
        ] {
            let err = check_transport(&ep(u), false).unwrap_err().to_string();
            assert!(
                err.contains("--insecure-http") && err.contains("tailnet"),
                "{err}"
            );
            assert!(check_transport(&ep(u), true).unwrap(), "{u}: warn");
        }
    }

    /// The command refuses before it reads a password or sends a request.
    #[tokio::test]
    async fn login_refuses_plain_http_to_another_host() {
        let (_tmp, s) = store();
        let mock = MockTransport::new();
        let c = ctx(&mock, &s, TokenSource::None, "pw\n");
        let err = login(&c, true, false).await.unwrap_err();
        assert!(err.to_string().contains("clear text"), "{err}");
        assert!(mock.requests().is_empty());
        assert!(s.load().unwrap().is_empty());
    }

    #[tokio::test]
    async fn login_needs_a_tcp_url() {
        let (_tmp, s) = store();
        let mock = MockTransport::new();
        let mut c = ctx(&mock, &s, TokenSource::None, "pw\n");
        c.http = Some(HttpTransport::new(Endpoint::Unix("/x.sock".into()), None));
        let err = login(&c, true, false).await.unwrap_err();
        assert!(err.to_string().contains("TERRARIUM_URL"), "{err}");
    }
}
