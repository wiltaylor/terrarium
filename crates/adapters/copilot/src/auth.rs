//! Auth status for Copilot (`docs/compat.md`, "Auth status").
//!
//! Copilot has no `auth status` command. Its token is in one of three
//! places, in this order of precedence:
//!
//! 1. An env var: `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`
//!    (verified from `copilot help environment`; the override itself is
//!    not tested).
//! 2. The system keychain (libsecret over D-Bus). This is the normal case
//!    on a desktop. A sandbox cannot reach it: with no D-Bus, Copilot
//!    starts logged out ("Please use /login to sign in to use Copilot").
//! 3. The plain-text fallback in `$COPILOT_HOME/config.json`, which
//!    `copilot login` offers when there is no keychain ("Store token in
//!    plain text config file?"). This is the normal case in the container.
//!
//! `config.json` is JSON with `//` comment lines at the top, so a strict
//! JSON parser refuses it. A real container login (2026-10-25,
//! `tests/fixtures/auth/config-plaintext-login.json`, sanitised) wrote:
//!
//! ```text
//! // User settings belong in settings.json.
//! // This file is managed automatically.
//! {
//!   "authTokens": {
//!     "https://github.com:<login>:github": {"token": "gho_…"},
//!     "https://github.com:<login>": {"token": "gho_…"}
//!   },
//!   "loggedInUsers": [{"host": "https://github.com", "login": "<login>", "kind": "…"}],
//!   "lastLoggedInUser": {"host": "https://github.com", "login": "<login>"}
//! }
//! ```
//!
//! The two `authTokens` entries held the same token. `copilotTokens` (a
//! map of key to token string) is read from the binary and is not seen.
//!
//! [`auth_status`] checks 1 and 3 from files and env. The screen check
//! [`screen_auth`] reads what the TUI says at start (or after `/user`).

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{CONFIG_FILE, parse_jsonc};
use crate::error::{CopilotError, Result};

/// The text that the TUI prints when it has no usable token.
pub const LOGIN_REQUIRED: &str = "Please use /login to sign in";
/// The start of the `/user` answer, and of the start-up line.
pub const SIGNED_IN: &str = "Signed in to ";

/// Where the token comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CredentialSource {
    /// An env var with this name is set.
    Env(String),
    /// A token in the plain-text `config.json` fallback.
    PlainTextConfig,
    /// `config.json` names a login but holds no token: the token is in the
    /// system keychain, which a sandbox cannot reach.
    Keychain,
    /// No token found.
    None,
}

/// The auth state as far as files and env tell it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatus {
    pub source: CredentialSource,
    /// The GitHub login, when `config.json` names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// The GitHub host, for example `https://github.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl AuthStatus {
    /// True when an agent in a sandbox can use this login: the token is in
    /// the env or in the shared home. A keychain login is not enough.
    pub fn usable_in_sandbox(&self) -> bool {
        matches!(
            self.source,
            CredentialSource::Env(_) | CredentialSource::PlainTextConfig
        )
    }
}

/// What `config.json` says about the login.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigAuth {
    /// An entry with a non-empty token in `authTokens` or `copilotTokens`.
    pub has_token: bool,
    pub login: Option<String>,
    pub host: Option<String>,
}

fn user(v: &Value) -> (Option<String>, Option<String>) {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    (s("login"), s("host"))
}

/// The keys of the token maps in `config.json`.
const TOKEN_MAPS: [&str; 2] = ["authTokens", "copilotTokens"];

/// The token string of one token map entry: `{"token": "…"}` (seen) or a
/// bare string (`copilotTokens`, not seen).
fn entry_token(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s.as_str()),
        Value::Object(m) => m.get("token").and_then(Value::as_str),
        _ => None,
    }
    .filter(|t| !t.trim().is_empty())
}

/// `(host, login)` from a token map key, `https://github.com:<login>` or
/// `https://github.com:<login>:github`.
fn key_user(key: &str) -> Option<(String, String)> {
    let (scheme, rest) = key.split_once("://")?;
    let mut parts = rest.split(':');
    let host = parts.next().filter(|h| !h.is_empty())?;
    let login = parts.next().filter(|l| !l.is_empty())?;
    Some((format!("{scheme}://{host}"), login.to_owned()))
}

/// The token in the plain-text fallback of a parsed `config.json`. The
/// entry of `lastLoggedInUser` wins; else the first entry with a token.
pub fn token_in_config(v: &Value) -> Option<String> {
    let want = v.get("lastLoggedInUser").map(user);
    let entries = || {
        TOKEN_MAPS
            .iter()
            .filter_map(|k| v.get(*k).and_then(Value::as_object))
            .flat_map(|m| m.iter())
    };
    let wanted = |key: &str| match (&want, key_user(key)) {
        (Some((Some(l), Some(h))), Some((kh, kl))) => *l == kl && *h == kh,
        _ => false,
    };
    entries()
        .find(|(k, e)| wanted(k) && entry_token(e).is_some())
        .or_else(|| entries().find(|(_, e)| entry_token(e).is_some()))
        .and_then(|(_, e)| entry_token(e))
        .map(str::to_owned)
}

/// Read the login fields of a `config.json` text. A file that does not
/// parse gives the default (no token, no login).
pub fn parse_config(text: &str) -> ConfigAuth {
    let Some(v) = parse_jsonc(text) else {
        return ConfigAuth::default();
    };
    let from_key = || {
        TOKEN_MAPS
            .iter()
            .filter_map(|k| v.get(*k).and_then(Value::as_object))
            .flat_map(|m| m.keys())
            .find_map(|k| key_user(k))
            .map(|(h, l)| (Some(l), Some(h)))
    };
    let (login, host) = v
        .get("lastLoggedInUser")
        .map(user)
        .or_else(|| {
            v.get("loggedInUsers")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .map(user)
        })
        .filter(|(l, _)| l.is_some())
        .or_else(from_key)
        .unwrap_or((None, None));
    ConfigAuth {
        has_token: token_in_config(&v).is_some(),
        login,
        host,
    }
}

/// The auth status of the shared home `home`, with `env` as the env the
/// agent gets (for example the daemon's credential store, see
/// [`crate::CopilotAdapter::required_env`]).
pub fn auth_status(
    home: &Path,
    token_vars: &[&str],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<AuthStatus> {
    let path = home.join(CONFIG_FILE);
    let cfg = match std::fs::read_to_string(&path) {
        Ok(text) => parse_config(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigAuth::default(),
        Err(e) => return Err(CopilotError::io(path, e)),
    };
    let source = if let Some(var) = token_vars
        .iter()
        .find(|v| env(v).is_some_and(|t| !t.trim().is_empty()))
    {
        CredentialSource::Env((*var).to_owned())
    } else if cfg.has_token {
        CredentialSource::PlainTextConfig
    } else if cfg.login.is_some() {
        CredentialSource::Keychain
    } else {
        CredentialSource::None
    };
    Ok(AuthStatus {
        source,
        login: cfg.login,
        host: cfg.host,
    })
}

/// What the TUI says about the login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScreenAuth {
    /// "Signed in to https://github.com as <login> (GitHub.com)."
    SignedIn { host: String, login: String },
    /// "Please use /login to sign in to use Copilot".
    LoginRequired,
}

/// Read the login state from screen lines. `None` when the screen says
/// neither. The last line that says something wins.
pub fn screen_auth<S: AsRef<str>>(lines: &[S]) -> Option<ScreenAuth> {
    lines.iter().rev().find_map(|l| {
        let l = l.as_ref();
        if l.contains(LOGIN_REQUIRED) {
            return Some(ScreenAuth::LoginRequired);
        }
        let rest = &l[l.find(SIGNED_IN)? + SIGNED_IN.len()..];
        let (host, rest) = rest.split_once(" as ")?;
        let login = rest.split_whitespace().next()?.trim_end_matches('.');
        Some(ScreenAuth::SignedIn {
            host: host.trim().to_owned(),
            login: login.to_owned(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<String> {
        let path = format!(
            "{}/tests/fixtures/screens/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path)
            .expect("fixture")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    const VARS: &[&str] = &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

    #[test]
    fn captured_screens() {
        assert_eq!(
            screen_auth(&fixture("auth-signed-in.txt")),
            Some(ScreenAuth::SignedIn {
                host: "https://github.com".into(),
                login: "<login>".into(),
            })
        );
        assert_eq!(
            screen_auth(&fixture("auth-no-keychain.txt")),
            Some(ScreenAuth::LoginRequired)
        );
        assert_eq!(screen_auth(&fixture("screen-idle-nongit.txt")), None);
    }

    #[test]
    fn captured_plaintext_login() {
        // A real `copilot login` in the container, tokens and login
        // replaced. Comment lines first, then `authTokens` entries of
        // `{"token": …}`.
        let path = format!(
            "{}/tests/fixtures/auth/config-plaintext-login.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).expect("fixture");
        assert!(
            serde_json::from_str::<Value>(&text).is_err(),
            "not strict JSON"
        );
        let cfg = parse_config(&text);
        assert!(cfg.has_token);
        assert_eq!(cfg.login.as_deref(), Some("octo"));
        assert_eq!(cfg.host.as_deref(), Some("https://github.com"));
        let v = parse_jsonc(&text).expect("jsonc");
        assert_eq!(token_in_config(&v).as_deref(), Some("gho_REDACTED"));

        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::write(tmp.path().join(CONFIG_FILE), &text).expect("w");
        let st = auth_status(tmp.path(), VARS, &|_| None).expect("s");
        assert_eq!(st.source, CredentialSource::PlainTextConfig);
        assert!(st.usable_in_sandbox());

        // Empty tokens are not a login; the key still names the user.
        let empty = r#"{"authTokens": {"https://github.com:octo": {"token": ""}}}"#;
        let cfg = parse_config(empty);
        assert!(!cfg.has_token);
        assert_eq!(cfg.login.as_deref(), Some("octo"));
        // The entry of `lastLoggedInUser` wins over the others.
        let two = r#"{"authTokens": {"https://github.com:a": {"token": "ta"},
            "https://github.com:b": {"token": "tb"}},
            "lastLoggedInUser": {"host": "https://github.com", "login": "b"}}"#;
        assert_eq!(
            token_in_config(&parse_jsonc(two).expect("j")).as_deref(),
            Some("tb")
        );
    }

    #[test]
    fn config_fallback_and_env() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path();
        let none = |_: &str| None;
        assert_eq!(
            auth_status(home, VARS, &none).expect("s").source,
            CredentialSource::None
        );
        // The host case: a login, the token in the keychain.
        std::fs::write(
            home.join(CONFIG_FILE),
            "// User settings belong in settings.json.\n// This file is managed automatically.\n{\n  \"firstLaunchAt\": \"2026-03-11T00:00:00.000Z\",\n  \"lastLoggedInUser\": {\"host\": \"https://github.com\", \"login\": \"octo\"},\n  \"loggedInUsers\": [{\"host\": \"https://github.com\", \"login\": \"octo\"}]\n}\n",
        )
        .expect("w");
        let st = auth_status(home, VARS, &none).expect("s");
        assert_eq!(st.source, CredentialSource::Keychain);
        assert_eq!(st.login.as_deref(), Some("octo"));
        assert!(!st.usable_in_sandbox());
        // The container case: the plain-text fallback.
        std::fs::write(
            home.join(CONFIG_FILE),
            r#"{"loggedInUsers": [{"host": "https://github.com", "login": "octo"}], "copilotTokens": {"https://github.com:octo": "gho_x"}}"#,
        )
        .expect("w");
        let st = auth_status(home, VARS, &none).expect("s");
        assert_eq!(st.source, CredentialSource::PlainTextConfig);
        assert!(st.usable_in_sandbox());
        assert_eq!(st.host.as_deref(), Some("https://github.com"));
        // An env token wins; an empty one does not count.
        let env = |k: &str| match k {
            "GITHUB_TOKEN" => Some("x".to_owned()),
            "COPILOT_GITHUB_TOKEN" => Some(" ".to_owned()),
            _ => None,
        };
        assert_eq!(
            auth_status(home, VARS, &env).expect("s").source,
            CredentialSource::Env("GITHUB_TOKEN".into())
        );
        assert_eq!(parse_config("garbage"), ConfigAuth::default());
    }
}
