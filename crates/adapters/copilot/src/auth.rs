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
//!    The key names (`copilotTokens`, `authTokens`, `loggedInUsers`,
//!    `lastLoggedInUser`) are read from the binary; the file layout is not
//!    tested.
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
    /// A non-empty `copilotTokens` or `authTokens`.
    pub has_token: bool,
    pub login: Option<String>,
    pub host: Option<String>,
}

fn non_empty(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Object(m)) => !m.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::String(s)) => !s.is_empty(),
        _ => false,
    }
}

fn user(v: &Value) -> (Option<String>, Option<String>) {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    (s("login"), s("host"))
}

/// Read the login fields of a `config.json` text. A file that does not
/// parse gives the default (no token, no login).
pub fn parse_config(text: &str) -> ConfigAuth {
    let Some(v) = parse_jsonc(text) else {
        return ConfigAuth::default();
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
        .unwrap_or((None, None));
    ConfigAuth {
        has_token: non_empty(v.get("copilotTokens")) || non_empty(v.get("authTokens")),
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
