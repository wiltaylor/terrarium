//! SSH keys for targets (PLAN §16b.6a, docs/cli.md): `secret generate`,
//! `targets show`, and `targets test`.

use serde_json::{Value, json};

use super::set_opt;
use crate::cli::SecretGenerateArgs;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

/// The scope of a secret: `--project`, else `global`.
fn scope(ctx: &Ctx) -> String {
    ctx.project.clone().unwrap_or_else(|| "global".to_owned())
}

/// `secret generate <name>`: the public key line first, so that
/// `terrarium secret generate k | head -1 >> authorized_keys` works.
pub async fn generate(ctx: &Ctx, a: &SecretGenerateArgs) -> anyhow::Result<()> {
    let path = format!(
        "/api/secrets/{}/{}/generate",
        encode(&scope(ctx)),
        encode(&a.name)
    );
    let mut body = json!({});
    set_opt(&mut body, "type", a.key_type.clone());
    set_opt(
        &mut body,
        "bits",
        a.bits.as_deref().and_then(|b| b.parse::<u64>().ok()),
    );
    set_opt(&mut body, "comment", a.comment.clone());
    if a.replace {
        body["replace"] = true.into();
    }
    let v = ctx.api.post(&path, Some(body)).await?;
    ctx.emit(&v, |c, v| {
        c.line(field(&v["fields"], "public_key"));
        c.line(format!(
            "fingerprint {} of secret {}/{}: add the line above to ~/.ssh/authorized_keys on \
             the target",
            field(&v["fields"], "fingerprint"),
            field(v, "scope"),
            field(v, "name")
        ));
    })
}

/// The KEY cell of a target: the credential name and the fingerprint of
/// its key, `!` and the warning when it does not work, or `-`.
pub fn key_cell(t: &Value) -> String {
    let c = &t["credential_info"];
    let Some(name) = c["name"].as_str() else {
        return "-".to_owned();
    };
    let mut s = name.to_owned();
    if let Some(f) = c["fingerprint"].as_str() {
        s.push(' ');
        s.push_str(f);
    }
    if c["warning"].is_string() {
        s.push_str(" (!)");
    }
    s
}

/// `targets show <name>`.
pub async fn show(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
    let v = ctx
        .api
        .get(&format!("/api/targets/{}", encode(name)), vec![])
        .await?;
    ctx.emit(&v, |c, t| {
        let mut rows: Vec<(&str, String)> = vec![
            ("name", field(t, "name")),
            ("kind", field(t, "kind")),
            ("scope", field(t, "scope")),
        ];
        for k in [
            "project",
            "host",
            "address",
            "port",
            "username",
            "domain",
            "image",
            "container",
            "lab",
            "machine",
            "lab_repo",
        ] {
            if !t[k].is_null() {
                rows.push((k, field(t, k)));
            }
        }
        rows.push(("source", field(t, "source")));
        rows.push(("state", field(t, "state")));
        let li = &t["limits_info"];
        if let Some(sum) = li["summary"].as_str() {
            let per = match li["per"].as_str() {
                Some("container") => "one budget for the container",
                _ => "for each agent",
            };
            rows.push(("limits", format!("{sum} ({per})")));
            if let Some(p) = li["problem"].as_str() {
                rows.push(("warning", p.to_owned()));
            }
        }
        let ci = &t["credential_info"];
        if ci.is_object() {
            let mut cred = field(ci, "name");
            match (ci["scope"].as_str(), ci["kind"].as_str()) {
                (Some(s), Some(k)) => cred.push_str(&format!(" ({k}, {s})")),
                _ => cred.push_str(" (missing)"),
            }
            rows.push(("credential", cred));
            if let Some(f) = ci["fingerprint"].as_str() {
                rows.push(("fingerprint", f.to_owned()));
            }
            if let Some(w) = ci["warning"].as_str() {
                rows.push(("warning", w.to_owned()));
            }
        }
        for (k, v) in rows {
            c.line(format!("{:<12} {v}", format!("{k}:")));
        }
    })
}

/// `targets test <name>`: exit 1 when the result is not `ok`.
pub async fn test(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
    let v = ctx
        .api
        .post(&format!("/api/targets/{}/test", encode(name)), None)
        .await?;
    ctx.emit(&v, |c, r| {
        c.line(format!(
            "{name}: {}: {}",
            field(r, "result"),
            field(r, "message")
        ));
        if let Some(k) = r["credential"].as_str() {
            let f = r["fingerprint"].as_str().unwrap_or("-");
            c.line(format!("key:      {k} {f}"));
        }
        c.line(format!("host key: {}", field(r, "host_key")));
        c.line(format!("policy:   {}", field(r, "host_key_policy")));
        if let Some(e) = r["stderr"].as_str().filter(|e| !e.is_empty()) {
            c.line("ssh said:");
            for l in e.lines() {
                c.line(format!("  {l}"));
            }
        }
    })?;
    match v["result"].as_str() {
        Some("ok") => Ok(()),
        r => Err(anyhow::anyhow!(
            "target test of {name}: {}",
            r.unwrap_or("no result")
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use clap::Parser;
    use serde_json::{Value, json};

    use crate::cli::Cli;
    use crate::ctx::Ctx;
    use crate::transport::Method;
    use crate::transport::mock::MockTransport;

    async fn run(args: &[&str], reply: Value) -> (anyhow::Result<()>, String, MockTransport) {
        let cli =
            Cli::try_parse_from(std::iter::once("terrarium").chain(args.iter().copied())).unwrap();
        let mock = MockTransport::new();
        mock.reply(200, reply);
        let mut ctx = Ctx::for_test(Arc::new(mock.clone()), None);
        ctx.json = cli.json;
        ctx.project = cli.project.clone();
        let r = crate::commands::run(&ctx, &cli.command).await;
        (r, ctx.out.captured(), mock)
    }

    #[tokio::test]
    async fn generate_posts_the_options_and_prints_the_public_key_first() {
        let info = json!({"scope": "p", "name": "deploy", "kind": "ssh_key",
            "fields": {"public_key": "ssh-rsa AAAAB3 ci@p", "fingerprint": "SHA256:abc"}});
        let (r, out, mock) = run(
            &[
                "-p",
                "p",
                "secret",
                "generate",
                "deploy",
                "--type",
                "rsa",
                "--bits",
                "4096",
                "--comment",
                "ci@p",
            ],
            info.clone(),
        )
        .await;
        r.unwrap();
        let reqs = mock.requests();
        assert_eq!(reqs[0].method, Method::Post);
        assert_eq!(reqs[0].path, "/api/secrets/p/deploy/generate");
        assert_eq!(
            reqs[0].body,
            Some(json!({"type": "rsa", "bits": 4096, "comment": "ci@p"}))
        );
        assert_eq!(out.lines().next(), Some("ssh-rsa AAAAB3 ci@p"), "{out}");
        assert!(out.contains("SHA256:abc"), "{out}");

        let (r, out, mock) = run(&["--json", "secret", "generate", "k"], info).await;
        r.unwrap();
        assert_eq!(mock.requests()[0].path, "/api/secrets/global/k/generate");
        assert_eq!(mock.requests()[0].body, Some(json!({})));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["fields"]["fingerprint"], "SHA256:abc");
        // A bad type stops in clap.
        assert!(
            Cli::try_parse_from(["terrarium", "secret", "generate", "k", "--type", "dsa"]).is_err()
        );
    }

    #[tokio::test]
    async fn target_test_prints_the_result_and_fails_when_not_ok() {
        let report = json!({"target": "box", "result": "auth_refused",
            "message": "the host refused the key", "host_key": "known",
            "host_key_policy": "trust on first use", "credential": "deploy",
            "fingerprint": "SHA256:abc", "exit_code": 255,
            "stderr": "build@box: Permission denied (publickey)."});
        let (r, out, mock) = run(&["target", "test", "box"], report).await;
        assert!(r.is_err());
        assert_eq!(mock.requests()[0].path, "/api/targets/box/test");
        assert!(
            out.contains("box: auth_refused: the host refused the key"),
            "{out}"
        );
        assert!(out.contains("deploy SHA256:abc"), "{out}");
        assert!(out.contains("Permission denied (publickey)."), "{out}");
        let (r, _, _) = run(
            &["target", "test", "box"],
            json!({"result": "ok", "message": "the login works", "host_key": "known"}),
        )
        .await;
        r.unwrap();
    }

    #[tokio::test]
    async fn target_show_names_the_key_and_its_fingerprint() {
        let t = json!({"name": "box", "kind": "ssh", "scope": "global", "address": "b@box",
            "source": "api", "state": "unknown", "credential": "deploy",
            "credential_info": {"name": "deploy", "scope": "global", "kind": "ssh_key",
                                "fingerprint": "SHA256:abc"}});
        let (r, out, _) = run(&["targets", "show", "box"], t).await;
        r.unwrap();
        assert!(
            out.contains("credential:  deploy (ssh_key, global)"),
            "{out}"
        );
        assert!(out.contains("fingerprint: SHA256:abc"), "{out}");
        let t = json!({"name": "box", "kind": "ssh", "credential_info": {"name": "gone",
            "scope": null, "kind": null, "warning": "no secret"}});
        let (_, out, _) = run(&["targets", "show", "box"], t.clone()).await;
        assert!(
            out.contains("gone (missing)") && out.contains("warning:     no secret"),
            "{out}"
        );
        assert_eq!(super::key_cell(&t), "gone (!)");
        assert_eq!(super::key_cell(&json!({})), "-");
    }
}
