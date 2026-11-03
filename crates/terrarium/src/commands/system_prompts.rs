//! `system-prompt`: the system prompt library (PLAN §8.1, docs/api.md
//! §28), and `agent set --save-system-prompt`.

use std::path::Path;

use anyhow::Context as _;
use serde_json::{Value, json};

use super::{agent_path, print_object_with_text, print_table};
use crate::api::CliError;
use crate::cli::SystemPromptCmd;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

fn path(name: &str) -> String {
    format!("/api/system-prompts/{}", encode(name))
}

/// The text of `--file`: a file, or stdin for `-`.
fn read_text(ctx: &Ctx, file: &Path) -> anyhow::Result<String> {
    if file.as_os_str() == "-" {
        return ctx.text("-");
    }
    std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))
}

/// One system prompt: the fields, the users, then the text.
fn print_one(ctx: &Ctx, v: &Value) {
    let mut head = v.clone();
    let users: Vec<Value> = head
        .as_object_mut()
        .and_then(|o| o.remove("users"))
        .and_then(|u| u.as_array().cloned())
        .unwrap_or_default();
    let names: Vec<String> = users
        .iter()
        .map(|u| {
            let by_role = if u["by_role"] == true {
                " (by role)"
            } else {
                ""
            };
            format!("{} {}{by_role}", field(u, "kind"), field(u, "name"))
        })
        .collect();
    head["users"] = if names.is_empty() {
        "none".into()
    } else {
        names.join(", ").into()
    };
    print_object_with_text(ctx, &head, "text");
}

pub async fn system_prompt(ctx: &Ctx, cmd: &SystemPromptCmd) -> anyhow::Result<()> {
    match cmd {
        SystemPromptCmd::Ls => {
            let v = ctx.api.get("/api/system-prompts", vec![]).await?;
            ctx.emit(&v, |c, v| {
                let mut items = v.as_array().cloned().unwrap_or_default();
                // `over limit`: an agent with this persona does not start.
                for o in items.iter_mut().filter_map(Value::as_object_mut) {
                    let over = o.get("over_limit").and_then(Value::as_bool) == Some(true);
                    o.insert(
                        "limit".into(),
                        Value::from(if over { "over limit" } else { "ok" }),
                    );
                }
                print_table(
                    c,
                    &items,
                    &[
                        ("NAME", "name"),
                        ("SOURCE", "source"),
                        ("SIZE", "size"),
                        ("LIMIT", "limit"),
                        ("DESCRIPTION", "description"),
                    ],
                    "no system prompts",
                );
            })
        }
        SystemPromptCmd::Show { name } => {
            let v = ctx.api.get(&path(name), vec![]).await?;
            ctx.emit(&v, print_one)
        }
        SystemPromptCmd::New {
            name,
            file,
            description,
            replace,
        } => {
            let mut body = json!({"name": name, "text": read_text(ctx, file)?});
            if let Some(d) = description {
                body["description"] = d.clone().into();
            }
            if *replace {
                body["replace"] = true.into();
            }
            let r = ctx.api.post("/api/system-prompts", Some(body)).await?;
            ctx.emit(&r, |c, r| {
                c.line(format!(
                    "saved system prompt {} ({}); select it with `--persona {}`; commit it with \
                     `terrarium admin config commit`",
                    field(r, "name"),
                    field(r, "path"),
                    field(r, "name")
                ));
            })
        }
        SystemPromptCmd::Edit {
            name,
            file,
            description,
        } => {
            let mut body = json!({});
            if let Some(f) = file {
                body["text"] = read_text(ctx, f)?.into();
            }
            if let Some(d) = description {
                body["description"] = d.clone().into();
            }
            let r = ctx.api.put(&path(name), body).await?;
            ctx.emit(&r, |c, r| {
                c.line(format!(
                    "updated system prompt {}; agents get it at their next start",
                    field(r, "name")
                ));
            })
        }
        SystemPromptCmd::Rm { name, force } => {
            let q = if *force {
                vec![("force".to_owned(), "true".to_owned())]
            } else {
                vec![]
            };
            let r = ctx.api.delete(&path(name), q).await?;
            ctx.emit(&r, |c, _| c.line(format!("deleted system prompt {name}")))
        }
    }
}

/// `agent set <agent> --save-system-prompt <name>`: save the agent's own
/// `system_prompt` in the library as `name`. The caller then selects it
/// (`persona = name`, the text cleared) in its patch.
pub async fn save_from_agent(ctx: &Ctx, agent: &str, name: &str) -> anyhow::Result<()> {
    let a = ctx.api.get(&agent_path(agent), vec![]).await?;
    let Some(text) = a["system_prompt"].as_str().filter(|t| !t.trim().is_empty()) else {
        return Err(CliError::usage(format!(
            "agent {agent} has no system prompt text of its own to save"
        )));
    };
    ctx.api
        .post(
            "/api/system-prompts",
            Some(json!({"name": name, "text": text})),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use clap::Parser;
    use serde_json::{Value, json};

    use crate::cli::{Cli, Command, SystemPromptCmd};
    use crate::ctx::Ctx;
    use crate::transport::Method;
    use crate::transport::mock::MockTransport;

    /// Run `args` against replies; the requests (method, path and query,
    /// body) and the output.
    async fn run(
        args: &[&str],
        replies: Vec<(u16, Value)>,
        stdin: Option<&str>,
    ) -> (
        anyhow::Result<()>,
        Vec<(Method, String, Option<Value>)>,
        String,
    ) {
        let cli = Cli::try_parse_from(std::iter::once("terrarium").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let mock = MockTransport::new();
        for (s, b) in replies {
            mock.reply(s, b);
        }
        let mut ctx = Ctx::for_test(Arc::new(mock.clone()), None);
        if let Some(s) = stdin {
            ctx = ctx.with_stdin(s);
        }
        let result = crate::commands::run(&ctx, &cli.command).await;
        let reqs = mock
            .requests()
            .into_iter()
            .map(|r| (r.method, r.path_and_query(), r.body))
            .collect();
        (result, reqs, ctx.out.captured())
    }

    #[test]
    fn commands_parse() {
        let cli = Cli::try_parse_from([
            "terrarium",
            "system-prompt",
            "new",
            "terse",
            "--file",
            "-",
            "--description",
            "d",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::SystemPrompt {
                command: SystemPromptCmd::New { .. }
            }
        ));
        for args in [
            vec!["system-prompts", "ls"],
            vec!["system-prompt", "show", "x"],
            vec!["system-prompt", "edit", "x", "--file", "f.md"],
            vec!["system-prompt", "edit", "x", "--description", ""],
            vec!["system-prompt", "rm", "x", "--force"],
            vec!["admin", "agent", "set", "a", "--save-system-prompt", "x"],
        ] {
            Cli::try_parse_from(std::iter::once("terrarium").chain(args.iter().copied()))
                .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        // `edit` needs a file or a description; `new` needs a file; the
        // save of the text does not go with another system prompt flag.
        for args in [
            vec!["system-prompt", "edit", "x"],
            vec!["system-prompt", "new", "x"],
            vec![
                "admin",
                "agent",
                "set",
                "a",
                "--save-system-prompt",
                "x",
                "--persona",
                "y",
            ],
            vec![
                "admin",
                "agent",
                "set",
                "a",
                "--save-system-prompt",
                "x",
                "--system-prompt",
                "t",
            ],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("terrarium").chain(args.iter().copied()))
                    .is_err(),
                "{args:?}"
            );
        }
    }

    #[tokio::test]
    async fn requests_and_output() {
        let entry = json!({"name": "terse", "description": "Short", "size": 40,
            "source": "config", "path": "system-prompts/terse.md", "builtin": false,
            "text": "Be terse.\n", "users": [{"kind": "agent", "id": "A", "name": "p/w"}]});
        let (r, reqs, out) = run(
            &["system-prompt", "ls"],
            vec![(200, json!([entry.clone()]))],
            None,
        )
        .await;
        r.unwrap();
        assert_eq!(reqs[0].1, "/api/system-prompts");
        assert!(out.contains("terse") && out.contains("Short"), "{out}");
        assert!(out.contains("LIMIT") && out.contains("ok"), "{out}");
        assert!(!out.contains("over limit"), "{out}");

        // A hand-placed file over the limit (issue #6).
        let big = json!({"name": "big", "description": "", "size": 41255, "source": "config",
            "path": "system-prompts/big.md", "builtin": false, "over_limit": true});
        let (r, _, out) = run(&["system-prompt", "ls"], vec![(200, json!([big]))], None).await;
        r.unwrap();
        assert!(out.contains("over limit"), "{out}");

        let (r, _, out) = run(
            &["system-prompt", "show", "terse"],
            vec![(200, entry.clone())],
            None,
        )
        .await;
        r.unwrap();
        assert!(out.contains("agent p/w"), "{out}");
        assert!(out.ends_with("text:\nBe terse.\n"), "{out}");

        let (r, reqs, out) = run(
            &[
                "system-prompt",
                "new",
                "terse",
                "--file",
                "-",
                "--description",
                "Short",
            ],
            vec![(201, entry.clone())],
            Some("Be terse.\n"),
        )
        .await;
        r.unwrap();
        assert_eq!(reqs[0].0, Method::Post);
        assert_eq!(
            reqs[0].2,
            Some(json!({"name": "terse", "text": "Be terse.\n", "description": "Short"}))
        );
        assert!(out.contains("--persona terse"), "{out}");

        let (r, reqs, _) = run(
            &["system-prompt", "edit", "terse", "--description", ""],
            vec![(200, entry.clone())],
            None,
        )
        .await;
        r.unwrap();
        assert_eq!(reqs[0].0, Method::Put);
        assert_eq!(reqs[0].2, Some(json!({"description": ""})));

        let (r, reqs, _) = run(
            &["system-prompt", "rm", "terse", "--force"],
            vec![(204, Value::Null)],
            None,
        )
        .await;
        r.unwrap();
        assert_eq!(reqs[0].1, "/api/system-prompts/terse?force=true");

        // Save the agent's own text, then select it.
        let (r, reqs, _) = run(
            &[
                "admin",
                "agent",
                "set",
                "p/w",
                "--save-system-prompt",
                "mine",
            ],
            vec![
                (
                    200,
                    json!({"id": "A", "project": "p", "name": "w", "system_prompt": "Mine."}),
                ),
                (201, entry.clone()),
                (
                    200,
                    json!({"id": "A", "project": "p", "name": "w", "persona": "mine"}),
                ),
            ],
            None,
        )
        .await;
        r.unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[1].2, Some(json!({"name": "mine", "text": "Mine."})));
        assert_eq!(reqs[2].0, Method::Patch);
        assert_eq!(
            reqs[2].2,
            Some(json!({"persona": "mine", "system_prompt": null}))
        );
        // No text of its own: nothing is saved.
        let (r, reqs, _) = run(
            &[
                "admin",
                "agent",
                "set",
                "p/w",
                "--save-system-prompt",
                "mine",
            ],
            vec![(200, json!({"id": "A", "project": "p", "name": "w"}))],
            None,
        )
        .await;
        assert!(r.is_err());
        assert_eq!(reqs.len(), 1);
    }
}
