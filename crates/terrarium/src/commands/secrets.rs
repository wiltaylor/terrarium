//! `secret`: the secret manager (docs/api.md §5.2, PLAN §16b). The scope
//! is `--project`, else `global`. A secret value never comes from an
//! argument: `set` reads it from stdin, from `--file`, or from a no-echo
//! prompt on a terminal.

use anyhow::Context as _;
use serde_json::{Map, Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::{SecretCmd, SecretSetArgs};
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

const GLOBAL: &str = "global";

/// The scope of a command: `--project`, else `global`.
fn scope(ctx: &Ctx) -> String {
    ctx.project.clone().unwrap_or_else(|| GLOBAL.to_owned())
}

fn path(scope: &str, name: &str) -> String {
    format!("/api/secrets/{}/{}", encode(scope), encode(name))
}

/// The plain fields as `k=v` text, for a table cell.
fn plain_text(s: &Value) -> String {
    s["fields"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| *k != "public_key")
                .map(|(k, v)| match v {
                    Value::String(t) => format!("{k}={t}"),
                    other => format!("{k}={other}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn print_list(ctx: &Ctx, v: &Value) {
    let rows: Vec<Value> = v
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut s| {
            s["plain"] = plain_text(&s).into();
            s["secret"] = s["secret_fields"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default()
                .into();
            s
        })
        .collect();
    print_table(
        ctx,
        &rows,
        &[
            ("SCOPE", "scope"),
            ("NAME", "name"),
            ("KIND", "kind"),
            ("FIELDS", "plain"),
            ("SECRET", "secret"),
            ("UPDATED", "updated_at"),
        ],
        "no secrets",
    );
}

fn print_one(ctx: &Ctx, s: &Value) {
    ctx.line(format!("{}/{}", field(s, "scope"), field(s, "name")));
    ctx.line(format!("  kind:     {}", field(s, "kind")));
    if let Some(m) = s["fields"].as_object() {
        for (k, v) in m {
            let t = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
            ctx.line(format!("  {k}: {t}"));
        }
    }
    let names: Vec<&str> = s["secret_fields"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    ctx.line(format!("  secret:   {} (not shown)", names.join(", ")));
    ctx.line(format!(
        "  updated:  {} by {}",
        field(s, "updated_at"),
        field(s, "created_by")
    ));
}

/// The value of the secret: `--file`, a no-echo prompt on a terminal,
/// else stdin.
fn read_value(ctx: &Ctx, a: &SecretSetArgs, label: &str) -> anyhow::Result<String> {
    if let Some(f) = &a.file {
        return std::fs::read_to_string(f).with_context(|| format!("cannot read {}", f.display()));
    }
    if ctx.stdin_is_terminal() {
        return rpassword::prompt_password(format!("{label}: "))
            .context("cannot read the value from the terminal");
    }
    ctx.text("-")
}

/// One line: the text without its last line end.
fn one_line(s: &str) -> String {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
        .to_owned()
}

/// `KEY=value` pairs.
fn pairs<'a>(
    lines: impl Iterator<Item = &'a str>,
    what: &str,
) -> anyhow::Result<Map<String, Value>> {
    let mut out = Map::new();
    for l in lines {
        let l = l.trim_end_matches('\r');
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else {
            return Err(CliError::usage(format!(
                "{what}: `{}` is not KEY=value",
                k_only(l)
            )));
        };
        out.insert(k.trim().to_owned(), v.to_owned().into());
    }
    Ok(out)
}

/// The part of a bad line that is safe to show: up to 20 characters of the
/// key side, never a value.
fn k_only(l: &str) -> String {
    l.chars().take(20).collect()
}

/// The body of `PUT /api/secrets/{scope}/{name}` from the flags and the
/// value.
fn set_body(ctx: &Ctx, a: &SecretSetArgs) -> anyhow::Result<Value> {
    let kind = a.kind.as_str();
    let mut fields = Map::new();
    let mut secret_fields: Vec<String> = Vec::new();
    let plain = |fields: &mut Map<String, Value>, k: &str, v: &Option<String>| {
        if let Some(v) = v {
            fields.insert(k.to_owned(), v.clone().into());
        }
    };
    let refuse = |flag: &str| {
        Err(CliError::usage(format!(
            "--{flag} is not a field of a {kind} secret"
        )))
    };
    if kind != "generic" && !a.fields.is_empty() {
        return refuse("field");
    }
    match kind {
        "api_key" => {
            for (f, set) in [
                ("username", a.username.is_some()),
                ("login-url", a.url.is_some()),
                ("domain", a.domain.is_some()),
                ("host", !a.hosts.is_empty()),
            ] {
                if set {
                    return refuse(f);
                }
            }
            fields.insert(
                "value".into(),
                one_line(&read_value(ctx, a, "API key")?).into(),
            );
            plain(&mut fields, "env", &a.env);
        }
        "token" | "github_token" => {
            for (f, set) in [
                ("login-url", a.url.is_some()),
                ("domain", a.domain.is_some()),
                ("env", a.env.is_some()),
            ] {
                if set {
                    return refuse(f);
                }
            }
            fields.insert(
                "value".into(),
                one_line(&read_value(ctx, a, "Token")?).into(),
            );
            if kind == "token" {
                plain(&mut fields, "username", &a.username);
                if !a.hosts.is_empty() {
                    fields.insert("hosts".into(), a.hosts.clone().into());
                }
            } else {
                if a.username.is_some() {
                    return refuse("username");
                }
                match a.hosts.as_slice() {
                    [] => {}
                    [h] => {
                        fields.insert("host".into(), h.clone().into());
                    }
                    _ => return Err(CliError::usage("a github_token has one --host")),
                }
            }
        }
        "login" => {
            for (f, set) in [("env", a.env.is_some()), ("host", !a.hosts.is_empty())] {
                if set {
                    return refuse(f);
                }
            }
            fields.insert(
                "password".into(),
                one_line(&read_value(ctx, a, "Password")?).into(),
            );
            plain(&mut fields, "username", &a.username);
            plain(&mut fields, "url", &a.url);
            plain(&mut fields, "domain", &a.domain);
        }
        "ssh_key" => {
            for (f, set) in [
                ("username", a.username.is_some()),
                ("login-url", a.url.is_some()),
                ("domain", a.domain.is_some()),
                ("env", a.env.is_some()),
                ("host", !a.hosts.is_empty()),
            ] {
                if set {
                    return refuse(f);
                }
            }
            if a.file.is_none() && ctx.stdin_is_terminal() {
                return Err(CliError::usage(
                    "give the private key with --file <path>, or on stdin",
                ));
            }
            fields.insert(
                "private_key".into(),
                read_value(ctx, a, "Private key")?.into(),
            );
            if let Some(p) = &a.passphrase_file {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("cannot read {}", p.display()))?;
                fields.insert("passphrase".into(), one_line(&text).into());
            }
        }
        "generic" => {
            for (f, set) in [
                ("username", a.username.is_some()),
                ("login-url", a.url.is_some()),
                ("domain", a.domain.is_some()),
                ("env", a.env.is_some()),
                ("host", !a.hosts.is_empty()),
            ] {
                if set {
                    return refuse(f);
                }
            }
            let plain_fields = pairs(a.fields.iter().map(String::as_str), "--field")?;
            if a.file.is_some() || !ctx.stdin_is_terminal() {
                let text = read_value(ctx, a, "Fields")?;
                let secret = pairs(text.lines(), "the secret fields")?;
                for (k, v) in secret {
                    if plain_fields.contains_key(&k) {
                        return Err(CliError::usage(format!(
                            "`{k}` is a --field and a secret field"
                        )));
                    }
                    secret_fields.push(k.clone());
                    fields.insert(k, v);
                }
            }
            fields.extend(plain_fields);
        }
        other => {
            return Err(CliError::usage(format!(
                "unknown kind `{other}`: api_key, token, github_token, login, ssh_key, or generic"
            )));
        }
    }
    if a.passphrase_file.is_some() && kind != "ssh_key" {
        return refuse("passphrase-file");
    }
    let mut body = json!({"kind": kind, "fields": fields});
    if kind == "generic" {
        body["secret_fields"] = secret_fields.into();
    }
    Ok(body)
}

pub async fn secret(ctx: &Ctx, cmd: &SecretCmd) -> anyhow::Result<()> {
    match cmd {
        SecretCmd::List => {
            let v = ctx
                .api
                .get("/api/secrets", query([("scope", ctx.project.clone())]))
                .await?;
            ctx.emit(&v, print_list)
        }
        SecretCmd::Show { name } => {
            let v = ctx.api.get(&path(&scope(ctx), name), vec![]).await?;
            ctx.emit(&v, print_one)
        }
        SecretCmd::Set(a) => {
            let body = set_body(ctx, a)?;
            let v = ctx.api.put(&path(&scope(ctx), &a.name), body).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "stored secret {}/{} ({})",
                    field(v, "scope"),
                    field(v, "name"),
                    field(v, "kind")
                ));
            })
        }
        SecretCmd::Rm { name } => {
            let s = scope(ctx);
            ctx.api.delete(&path(&s, name), vec![]).await?;
            ctx.emit(&json!({"deleted": format!("{s}/{name}")}), |c, _| {
                c.line(format!("deleted secret {s}/{name}"));
            })
        }
        SecretCmd::Reveal {
            name,
            password_stdin,
        } => {
            let password = if *password_stdin {
                ctx.text("-")?.lines().next().unwrap_or_default().to_owned()
            } else {
                rpassword::prompt_password("Operator password: ").context(
                    "cannot read the password from the terminal (use --password-stdin in a script)",
                )?
            };
            if password.is_empty() {
                return Err(CliError::usage("no operator password given"));
            }
            let v = ctx
                .api
                .post(
                    &format!("{}/reveal", path(&scope(ctx), name)),
                    Some(json!({"password": password})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{}/{} ({})",
                    field(v, "scope"),
                    field(v, "name"),
                    field(v, "kind")
                ));
                if let Some(m) = v["values"].as_object() {
                    for (k, val) in m {
                        let t = val.as_str().unwrap_or_default();
                        if t.contains('\n') {
                            c.line(format!("{k}:"));
                            c.line(t.trim_end());
                        } else {
                            c.line(format!("{k}: {t}"));
                        }
                    }
                }
            })
        }
    }
}
