//! The kinds of secrets and their fields (PLAN §16b.1). [`split`] checks the
//! fields of a kind and puts each one in the plain part (shown in each
//! list) or the secret part (sealed).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::error::{ApiError, ApiResult};

/// The user name that git sends with a token when the secret has none.
/// GitHub, GitLab, and Gitea accept any user name with a token.
pub const DEFAULT_TOKEN_USER: &str = "x-access-token";
/// The host of a `github_token` with no `host`.
pub const DEFAULT_GITHUB_HOST: &str = "github.com";
/// The largest secret field, in bytes.
pub const MAX_SECRET_LEN: usize = 64 * 1024;
/// The largest plain field, in bytes.
pub const MAX_PLAIN_LEN: usize = 1024;
/// The most fields of a `generic` secret.
pub const MAX_GENERIC_FIELDS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An API key: `value`, and an optional env var name `env`.
    ApiKey,
    /// An HTTPS git token: `username`, `value`, `hosts`.
    Token,
    /// A GitHub token: `value`, `host`.
    GithubToken,
    /// A user name and password, for `vnc` and `rdp` targets: `username`,
    /// `password`, `url`, `domain`.
    Login,
    /// An SSH private key: `private_key`, `passphrase`, and the derived
    /// `public_key`.
    SshKey,
    /// Named fields; each one is secret or plain.
    Generic,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::ApiKey,
        Kind::Token,
        Kind::GithubToken,
        Kind::Login,
        Kind::SshKey,
        Kind::Generic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Token => "token",
            Self::GithubToken => "github_token",
            Self::Login => "login",
            Self::SshKey => "ssh_key",
            Self::Generic => "generic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The secret fields of a typed kind, required first.
    fn secret_names(self) -> &'static [&'static str] {
        match self {
            Self::ApiKey | Self::Token | Self::GithubToken => &["value"],
            Self::Login => &["password"],
            Self::SshKey => &["private_key", "passphrase"],
            Self::Generic => &[],
        }
    }

    /// The plain fields that a caller may give for a typed kind.
    fn plain_names(self) -> &'static [&'static str] {
        match self {
            Self::ApiKey => &["env"],
            Self::Token => &["username", "hosts"],
            Self::GithubToken => &["host"],
            Self::Login => &["username", "url", "domain"],
            Self::SshKey | Self::Generic => &[],
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The secret fields of one secret, by name. Each value is zeroed on drop.
pub type SecretFields = BTreeMap<String, Zeroizing<String>>;

/// The fields of a new or changed secret, as the caller gives them:
/// `fields` holds the plain and the secret fields. For `generic`,
/// `secret_fields` names the secret ones. `Debug` shows only the names.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretInput {
    #[serde(default)]
    pub fields: Map<String, Value>,
    #[serde(default)]
    pub secret_fields: Vec<String>,
}

impl std::fmt::Debug for SecretInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretInput")
            .field("fields", &self.fields.keys().collect::<Vec<_>>())
            .field("secret_fields", &self.secret_fields)
            .finish()
    }
}

/// The fields of a secret after [`split`].
pub struct Split {
    pub plain: Map<String, Value>,
    pub secret: SecretFields,
}

impl std::fmt::Debug for Split {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Split")
            .field("plain", &self.plain)
            .field("secret", &self.secret.keys().collect::<Vec<_>>())
            .finish()
    }
}

fn invalid(m: impl Into<String>) -> ApiError {
    ApiError::invalid(m)
}

fn one_line(s: &str) -> bool {
    !s.contains(['\n', '\r', '\0'])
}

/// A plain text field: at most [`MAX_PLAIN_LEN`] bytes, no control
/// characters.
fn plain_text(kind: Kind, name: &str, v: &Value) -> ApiResult<String> {
    let Some(s) = v.as_str() else {
        return Err(invalid(format!("`{name}` of a {kind} is a string")));
    };
    if s.is_empty() || s.len() > MAX_PLAIN_LEN || s.chars().any(char::is_control) {
        return Err(invalid(format!(
            "`{name}` of a {kind} is 1 to {MAX_PLAIN_LEN} bytes with no control characters"
        )));
    }
    Ok(s.to_owned())
}

/// A secret text field. The value never goes into an error message.
fn secret_text(kind: Kind, name: &str, v: &Value) -> ApiResult<Zeroizing<String>> {
    let Some(s) = v.as_str() else {
        return Err(invalid(format!("`{name}` of a {kind} is a string")));
    };
    if s.is_empty() || s.len() > MAX_SECRET_LEN || s.contains('\0') {
        return Err(invalid(format!(
            "`{name}` of a {kind} is 1 to {MAX_SECRET_LEN} bytes with no NUL"
        )));
    }
    Ok(Zeroizing::new(s.to_owned()))
}

/// A one-line token: trimmed, with no line breaks (a line break would
/// inject lines into the git credential protocol).
fn token_value(kind: Kind, v: &Value) -> ApiResult<Zeroizing<String>> {
    let raw = secret_text(kind, "value", v)?;
    let t = raw.trim();
    if t.is_empty() {
        return Err(invalid(format!("the `value` of a {kind} is empty")));
    }
    if !one_line(t) {
        return Err(invalid(format!("the `value` of a {kind} is one line")));
    }
    Ok(Zeroizing::new(t.to_owned()))
}

fn host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-.:[]".contains(c))
}

fn env_ok(e: &str) -> bool {
    let mut cs = e.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && e.len() <= 128
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn field_name_ok(n: &str) -> bool {
    let mut cs = n.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && n.len() <= 64
        && cs.all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
}

/// Check the fields of `kind` and split them. `strict` is false only for
/// the migration of the old credential files (§16b.7): an `ssh_key` that
/// the parser does not read then keeps working as before, with no
/// `public_key`.
pub fn split(kind: Kind, input: &SecretInput, strict: bool) -> ApiResult<Split> {
    let mut plain = Map::new();
    let mut secret = SecretFields::new();
    if kind == Kind::Generic {
        return split_generic(input);
    }
    if !input.secret_fields.is_empty() {
        return Err(invalid(format!(
            "`secret_fields` is for a generic secret; a {kind} has fixed fields"
        )));
    }
    for name in input.fields.keys() {
        if !kind.secret_names().contains(&name.as_str())
            && !kind.plain_names().contains(&name.as_str())
        {
            let mut all: Vec<&str> = kind.secret_names().to_vec();
            all.extend(kind.plain_names());
            return Err(invalid(format!(
                "a {kind} has no field `{name}`; its fields are {}",
                all.join(", ")
            )));
        }
    }
    let f = |n: &str| input.fields.get(n).filter(|v| !v.is_null());
    let required = kind.secret_names()[0];
    let Some(main) = f(required) else {
        return Err(invalid(format!("a {kind} needs `{required}`")));
    };
    match kind {
        Kind::ApiKey => {
            secret.insert("value".into(), token_value(kind, main)?);
            if let Some(v) = f("env") {
                let e = plain_text(kind, "env", v)?;
                if !env_ok(&e) {
                    return Err(invalid(format!("`{e}` is not an env var name")));
                }
                plain.insert("env".into(), e.into());
            }
        }
        Kind::Token => {
            secret.insert("value".into(), token_value(kind, main)?);
            let user = match f("username") {
                Some(v) => plain_text(kind, "username", v)?,
                None => DEFAULT_TOKEN_USER.to_owned(),
            };
            plain.insert("username".into(), user.into());
            if let Some(v) = f("hosts") {
                let hosts: Vec<String> = match v {
                    Value::Array(a) => a
                        .iter()
                        .map(|h| plain_text(kind, "hosts", h))
                        .collect::<ApiResult<_>>()?,
                    Value::String(s) => s
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    _ => return Err(invalid("`hosts` of a token is a list of host names")),
                };
                let hosts: Vec<String> = hosts.into_iter().map(|h| h.to_lowercase()).collect();
                if let Some(bad) = hosts.iter().find(|h| !host_ok(h)) {
                    return Err(invalid(format!("`{bad}` is not a host name")));
                }
                if !hosts.is_empty() {
                    plain.insert("hosts".into(), hosts.into());
                }
            }
        }
        Kind::GithubToken => {
            secret.insert("value".into(), token_value(kind, main)?);
            let host = match f("host") {
                Some(v) => plain_text(kind, "host", v)?.to_lowercase(),
                None => DEFAULT_GITHUB_HOST.to_owned(),
            };
            if !host_ok(&host) {
                return Err(invalid(format!("`{host}` is not a host name")));
            }
            plain.insert("host".into(), host.into());
        }
        Kind::Login => {
            // Keep the password as it is: spaces can be part of it.
            let pw = secret_text(kind, "password", main)?;
            if pw.len() > MAX_PLAIN_LEN || !one_line(&pw) {
                return Err(invalid(format!(
                    "a password is one line of 1 to {MAX_PLAIN_LEN} bytes"
                )));
            }
            secret.insert("password".into(), pw);
            for n in ["username", "url", "domain"] {
                if let Some(v) = f(n) {
                    plain.insert(n.into(), plain_text(kind, n, v)?.into());
                }
            }
        }
        Kind::SshKey => {
            let mut key = secret_text(kind, "private_key", main)?;
            // ssh refuses a key file without a final newline.
            if !key.ends_with('\n') {
                key.push('\n');
            }
            let passphrase = match f("passphrase") {
                Some(v) => Some(secret_text(kind, "passphrase", v)?),
                None => None,
            };
            match super::ssh::public_key(&key, passphrase.as_deref().map(String::as_str)) {
                Ok(p) => {
                    if let Some(f) = super::ssh::fingerprint(&p) {
                        plain.insert("fingerprint".into(), f.into());
                    }
                    plain.insert("public_key".into(), p.into());
                }
                Err(e) if strict || passphrase.is_some() => return Err(e),
                Err(e) => {
                    if !key.contains("PRIVATE KEY-----") {
                        return Err(e);
                    }
                    tracing::warn!(
                        "secrets: an ssh_key moves in with no public key: {}",
                        e.message
                    );
                }
            }
            secret.insert("private_key".into(), key);
            if let Some(p) = passphrase {
                secret.insert("passphrase".into(), p);
            }
        }
        Kind::Generic => unreachable!("handled above"),
    }
    Ok(Split { plain, secret })
}

fn split_generic(input: &SecretInput) -> ApiResult<Split> {
    let kind = Kind::Generic;
    if input.fields.is_empty() {
        return Err(invalid("a generic secret needs at least one field"));
    }
    if input.fields.len() > MAX_GENERIC_FIELDS {
        return Err(invalid(format!(
            "a generic secret has at most {MAX_GENERIC_FIELDS} fields"
        )));
    }
    for s in &input.secret_fields {
        if !input.fields.contains_key(s) {
            return Err(invalid(format!(
                "`secret_fields` names `{s}`, which is not in `fields`"
            )));
        }
    }
    let mut plain = Map::new();
    let mut secret = SecretFields::new();
    for (name, v) in &input.fields {
        if !field_name_ok(name) {
            return Err(invalid(format!(
                "bad field name `{name}`: 1-64 of A-Z, a-z, 0-9, `_`, `-`, `.`, starting with a letter or `_`"
            )));
        }
        if input.secret_fields.contains(name) {
            secret.insert(name.clone(), secret_text(kind, name, v)?);
        } else {
            plain.insert(name.clone(), plain_text(kind, name, v)?.into());
        }
    }
    Ok(Split { plain, secret })
}

/// The names of the secret fields that a split has, in order.
pub fn secret_names(s: &SecretFields) -> Vec<String> {
    s.keys().cloned().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(v: Value) -> SecretInput {
        serde_json::from_value(v).unwrap()
    }

    fn fields(v: Value) -> SecretInput {
        input(json!({ "fields": v }))
    }

    #[test]
    fn typed_kinds_split_plain_and_secret_fields() {
        let s = split(
            Kind::Token,
            &fields(json!({"value": " ghp_x\n", "hosts": ["GitHub.com"]})),
            true,
        )
        .unwrap();
        assert_eq!(
            Value::Object(s.plain.clone()),
            json!({"username": "x-access-token", "hosts": ["github.com"]})
        );
        assert_eq!(s.secret["value"].as_str(), "ghp_x");
        assert!(!format!("{s:?}").contains("ghp_x"));

        let s = split(Kind::GithubToken, &fields(json!({"value": "t"})), true).unwrap();
        assert_eq!(Value::Object(s.plain), json!({"host": "github.com"}));

        let s = split(
            Kind::ApiKey,
            &fields(json!({"value": "sk-1", "env": "OPENAI_API_KEY"})),
            true,
        )
        .unwrap();
        assert_eq!(Value::Object(s.plain), json!({"env": "OPENAI_API_KEY"}));
        assert_eq!(secret_names(&s.secret), ["value"]);

        let s = split(
            Kind::Login,
            &fields(json!({"username": "Bob Smith", "password": " pass word ", "domain": "LAB"})),
            true,
        )
        .unwrap();
        assert_eq!(
            Value::Object(s.plain),
            json!({"username": "Bob Smith", "domain": "LAB"})
        );
        assert_eq!(s.secret["password"].as_str(), " pass word ");
    }

    #[test]
    fn generic_fields_are_secret_only_when_named() {
        let s = split(
            Kind::Generic,
            &input(json!({"fields": {"user": "bob", "pin": "1234", "key": "k"},
                          "secret_fields": ["pin", "key"]})),
            true,
        )
        .unwrap();
        assert_eq!(Value::Object(s.plain), json!({"user": "bob"}));
        assert_eq!(secret_names(&s.secret), ["key", "pin"]);
        // A secret field that is not in `fields`.
        assert!(
            split(
                Kind::Generic,
                &input(json!({"fields": {"a": "1"}, "secret_fields": ["b"]})),
                true
            )
            .is_err()
        );
        assert!(split(Kind::Generic, &fields(json!({})), true).is_err());
        assert!(split(Kind::Generic, &fields(json!({"bad name": "x"})), true).is_err());
    }

    #[test]
    fn bad_fields_are_refused_without_the_value() {
        let cases = [
            (Kind::Token, json!({"value": "a\nb"})),
            (Kind::Token, json!({"value": "   "})),
            (Kind::Token, json!({"username": "u"})),
            (Kind::Token, json!({"value": "v", "password": "p"})),
            (Kind::ApiKey, json!({"value": "v", "env": "1BAD"})),
            (Kind::Login, json!({"password": "a\nb"})),
            (Kind::Login, json!({"password": "p", "username": ""})),
            (Kind::GithubToken, json!({"value": "v", "host": "bad host"})),
            (Kind::SshKey, json!({"private_key": "not a key"})),
        ];
        for (kind, v) in cases {
            let e = split(kind, &fields(v.clone()), true).unwrap_err();
            assert_eq!(e.code, crate::error::Code::Invalid, "{kind} {v}");
            assert!(!e.message.contains("a\nb"), "{}", e.message);
        }
        // `secret_fields` is for generic only.
        assert!(
            split(
                Kind::Token,
                &input(json!({"fields": {"value": "v"}, "secret_fields": ["value"]})),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn kinds_round_trip_their_names() {
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), json!(k.as_str()));
        }
        assert_eq!(Kind::parse("password"), None);
    }
}
