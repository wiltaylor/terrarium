//! `mem` and `wiki`: PLAN §14, §15.1, and the §17 routes. The daemon does
//! not serve these routes yet.

use serde_json::{Value, json};

use super::{delimited, print_generic, print_table, set_opt, set_replace_list};
use crate::api::{CliError, ServerError, query};
use crate::cli::{MemCmd, MemNewArgs, WikiCmd};
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::{field, first_field, short_time};
use crate::transport::{Method, encode};

const MEM_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("TITLE", "title"),
    ("TAGS", "tags"),
    ("AUTHOR", "author_name"),
    ("UPDATED", "updated_at"),
];

fn mem_path(id: &str) -> String {
    format!("/api/memories/{}", encode(id))
}

/// Split `kind:ref`, at the first `:`.
fn split_pair<'a>(s: &'a str, what: &str) -> anyhow::Result<(&'a str, &'a str)> {
    s.split_once(':')
        .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        .ok_or_else(|| CliError::usage(format!("bad {what} `{s}`: use <a>:<b>")))
}

/// The rank of a relation in `show`: PLAN §14 puts disagreement first.
fn relation_rank(l: &Value) -> u8 {
    match l["relation"].as_str() {
        Some("contradicts") => 0,
        Some("supersedes") => 1,
        _ => 2,
    }
}

/// Print a memory, or a `/graph` answer: `{depth, nodes, edges}`, where
/// one node is the memory `id`.
fn print_memory(ctx: &Ctx, v: &Value, id: &str) {
    let nodes = as_items(&v["nodes"]);
    let m = nodes
        .iter()
        .find(|n| n["id"] == id || n["key"] == id)
        .or(nodes.first())
        .unwrap_or(v);
    ctx.line(format!("{}  {}", field(m, "id"), field(m, "title")));
    ctx.line(format!(
        "tags: {}  author: {}  updated: {}",
        field(m, "tags"),
        first_field(m, &["author_name", "author"]),
        short_time(&field(m, "updated_at"))
    ));
    if let Some(s) = m["superseded_by"].as_str() {
        ctx.line(format!("superseded by {s}"));
    }
    delimited(ctx, "MEMORY", m["body"].as_str().unwrap_or_default());
    let sources = as_items(&m["sources"]);
    if !sources.is_empty() {
        ctx.line("sources:");
        for s in sources {
            ctx.line(format!("  {}:{}", field(s, "kind"), field(s, "ref")));
        }
    }
    let mut links: Vec<&Value> = if v.get("edges").is_some() {
        as_items(&v["edges"]).iter().collect()
    } else {
        as_items(&m["links"]).iter().collect()
    };
    links.sort_by_key(|l| relation_rank(l));
    if !links.is_empty() {
        ctx.line("links:");
        for l in links {
            ctx.line(format!(
                "  {} -[{}]-> {}{}",
                first_field(l, &["from", "from_id"]),
                field(l, "relation"),
                first_field(l, &["to", "to_id"]),
                l["note"]
                    .as_str()
                    .map(|n| format!("  ({n})"))
                    .unwrap_or_default()
            ));
        }
    }
    let others: Vec<&Value> = nodes.iter().filter(|n| n["id"] != m["id"]).collect();
    if !others.is_empty() {
        ctx.line("linked memories:");
        for n in others {
            let gone = if n["superseded_by"].is_string() {
                "  (superseded)"
            } else {
                ""
            };
            ctx.line(format!("  {}  {}{gone}", field(n, "id"), field(n, "title")));
        }
    }
}

pub async fn mem(ctx: &Ctx, cmd: &MemCmd) -> anyhow::Result<()> {
    match cmd {
        MemCmd::Search {
            query: q,
            tag,
            limit,
            all,
        } => {
            let p = ctx.project_or(None).await?;
            let qs = query([
                ("q", Some(q.clone())),
                ("tag", tag.clone()),
                ("all", all.then(|| "true".to_owned())),
            ]);
            let v = Value::Array(
                ctx.api
                    .list(&format!("{}/memories", project_path(&p)), qs, Some(*limit))
                    .await?,
            );
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), MEM_COLS, "no memories")
            })
        }
        MemCmd::Show { id, depth } => {
            let v = if *depth == 0 {
                ctx.api.get(&mem_path(id), vec![]).await?
            } else {
                ctx.api
                    .get(
                        &format!("{}/graph", mem_path(id)),
                        query([("depth", Some(depth.to_string()))]),
                    )
                    .await?
            };
            ctx.emit(&v, |c, v| print_memory(c, v, id))
        }
        MemCmd::New(a) => new(ctx, a).await,
        MemCmd::Link {
            a,
            b,
            relation,
            note,
        } => {
            let mut body = json!({"to": b, "relation": relation});
            set_opt(&mut body, "note", note.clone());
            let r = ctx
                .api
                .post(&format!("{}/links", mem_path(a)), Some(body))
                .await?;
            ctx.emit(&r, |c, _| c.line(format!("linked {a} -[{relation}]-> {b}")))
        }
        MemCmd::Unlink { a, b, relation } => {
            let r = ctx
                .api
                .call(
                    Method::Delete,
                    &format!("{}/links", mem_path(a)),
                    vec![],
                    Some(json!({"to": b, "relation": relation})),
                )
                .await?;
            ctx.emit(&r, |c, _| {
                c.line(format!("unlinked {a} -[{relation}]-> {b}"));
            })
        }
        MemCmd::Edit {
            id,
            body,
            title,
            tags,
        } => {
            let mut b = json!({});
            set_opt(&mut b, "body", ctx.opt_text(body.as_deref())?);
            set_opt(&mut b, "title", title.clone());
            set_replace_list(&mut b, "tags", "--tag", tags)?;
            if b.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(CliError::usage("give --body, --title, or --tag"));
            }
            let m = ctx.api.patch(&mem_path(id), b).await?;
            ctx.emit(&m, |c, m| c.line(format!("updated {}", field(m, "id"))))
        }
        MemCmd::Supersede { old, new } => {
            let r = ctx
                .api
                .post(
                    &format!("{}/supersede", mem_path(old)),
                    Some(json!({"by": new})),
                )
                .await?;
            ctx.emit(&r, |c, _| c.line(format!("{old} is superseded by {new}")))
        }
        MemCmd::Neighbors { id, relation } => {
            let v = ctx
                .api
                .get(
                    &format!("{}/graph", mem_path(id)),
                    query([("depth", Some("1".into())), ("relation", relation.clone())]),
                )
                .await?;
            ctx.emit(&v, |c, v| print_memory(c, v, id))
        }
        MemCmd::Ls { tag, mine, recent } => {
            let p = ctx.project_or(None).await?;
            let author = if *mine { Some(ctx.me().await?) } else { None };
            let qs = query([("tag", tag.clone()), ("author", author)]);
            let v = Value::Array(
                ctx.api
                    .list(&format!("{}/memories", project_path(&p)), qs, Some(*recent))
                    .await?,
            );
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), MEM_COLS, "no memories")
            })
        }
        MemCmd::Rm { id } => {
            ctx.api.delete(&mem_path(id), vec![]).await?;
            ctx.emit(&json!({"deleted": id}), |c, _| {
                c.line(format!("deleted {id}"));
            })
        }
    }
}

async fn new(ctx: &Ctx, a: &MemNewArgs) -> anyhow::Result<()> {
    let p = ctx.project_or(None).await?;
    let path = format!("{}/memories", project_path(&p));
    let mut sources = Vec::new();
    for s in &a.sources {
        let (kind, r) = split_pair(s, "--source")?;
        sources.push(json!({"kind": kind, "ref": r}));
    }
    let mut links = Vec::new();
    for l in &a.links {
        // The relation follows the last `:`, so an ID may hold a `:`.
        let (to, relation) = l
            .rsplit_once(':')
            .filter(|(a, b)| !a.is_empty() && !b.is_empty())
            .ok_or_else(|| CliError::usage(format!("bad --link `{l}`: use <id>:<relation>")))?;
        links.push(json!({"to": to, "relation": relation}));
    }
    // PLAN §14: show the top hits for the title first, so that the agent
    // sees duplicates.
    if !ctx.json
        && let Ok(hits) = ctx
            .api
            .list(&path, query([("q", Some(a.title.clone()))]), Some(5))
            .await
        && !hits.is_empty()
    {
        ctx.line("similar memories (link to one of them instead of a duplicate?):");
        for h in &hits {
            ctx.line(format!("  {}  {}", field(h, "id"), field(h, "title")));
        }
    }
    let mut b = json!({"title": a.title});
    set_opt(&mut b, "body", ctx.opt_text(a.body.as_deref())?);
    set_replace_list(&mut b, "tags", "--tag", &a.tags)?;
    if !sources.is_empty() {
        b["sources"] = Value::Array(sources);
    }
    if !links.is_empty() {
        b["links"] = Value::Array(links);
    }
    let m = ctx.api.post(&path, Some(b)).await?;
    ctx.emit(&m, |c, m| {
        c.line(format!("created {}: {}", field(m, "id"), field(m, "title")));
    })
}

/// `/api/wiki/{key}`. The key is `project/path`, encoded as one segment,
/// as for agents (`docs/api.md` §2.2).
fn wiki_path(project: &str, page: &str) -> String {
    format!("/api/wiki/{}", encode(&format!("{project}/{page}")))
}

const WIKI_COLS: &[(&str, &str)] = &[
    ("PATH", "path"),
    ("TITLE", "title"),
    ("REV", "rev"),
    ("UPDATED", "updated_at"),
];

fn print_page(ctx: &Ctx, v: &Value) {
    ctx.line(format!(
        "{}  rev {}  by {}",
        first_field(v, &["path", "page"]),
        first_field(v, &["rev", "current_rev"]),
        first_field(v, &["author_name", "author"])
    ));
    ctx.out.write_str(v["body"].as_str().unwrap_or_default());
    if !v["body"].as_str().unwrap_or_default().ends_with('\n') {
        ctx.line("");
    }
}

fn print_saved(ctx: &Ctx, v: &Value) {
    ctx.line(format!(
        "saved {} rev {}",
        first_field(v, &["path", "page"]),
        first_field(v, &["rev", "current_rev"])
    ));
    if let Some(u) = v["url"].as_str() {
        ctx.line(u);
    }
}

fn print_diff(ctx: &Ctx, v: &Value) {
    match v {
        Value::String(s) => ctx.out.write_str(s),
        v => match v["diff"].as_str() {
            Some(d) => ctx.out.write_str(d),
            None => print_generic(ctx, v, "no changes"),
        },
    }
}

pub async fn wiki(ctx: &Ctx, cmd: &WikiCmd) -> anyhow::Result<()> {
    let p = ctx.project_or(None).await?;
    let list_path = format!("{}/wiki", project_path(&p));
    match cmd {
        WikiCmd::Ls { path } => {
            // The daemon lists the whole tree; the CLI keeps the paths under
            // the prefix.
            let mut items = ctx.api.list(&list_path, vec![], None).await?;
            if let Some(prefix) = path {
                let prefix = prefix.trim_end_matches('/');
                items.retain(|pg| {
                    pg["path"].as_str().is_some_and(|p| {
                        p == prefix || p.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
                    })
                });
            }
            let v = Value::Array(items);
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), WIKI_COLS, "no pages")
            })
        }
        WikiCmd::Show { page, rev } => {
            let path = match rev {
                Some(r) => format!("{}/revisions/{r}", wiki_path(&p, page)),
                None => wiki_path(&p, page),
            };
            let v = ctx.api.get(&path, vec![]).await?;
            ctx.emit(&v, print_page)
        }
        WikiCmd::Search { query: q } => {
            let v = Value::Array(
                ctx.api
                    .list(&list_path, query([("q", Some(q.clone()))]), None)
                    .await?,
            );
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), WIKI_COLS, "no pages")
            })
        }
        WikiCmd::New {
            page,
            body,
            title,
            summary,
        } => {
            let mut b = json!({"path": page, "body": ctx.text(body)?});
            set_opt(&mut b, "title", title.clone());
            set_opt(&mut b, "summary", summary.clone());
            let v = ctx.api.post(&list_path, Some(b)).await?;
            ctx.emit(&v, print_saved)
        }
        WikiCmd::Edit {
            page,
            body,
            base,
            summary,
        } => {
            let mut b = json!({"body": ctx.text(body)?, "base": base});
            set_opt(&mut b, "summary", summary.clone());
            let key = wiki_path(&p, page);
            match ctx.api.put(&key, b).await {
                Ok(v) => ctx.emit(&v, print_saved),
                Err(e) => {
                    // PLAN §15.1: a stale base prints the diff since the base.
                    // The daemon puts it in `details.diff`.
                    if !ctx.json
                        && let Some(s) = e.downcast_ref::<ServerError>()
                        && s.status == 409
                    {
                        let d = s.details.clone().unwrap_or_default();
                        if let Some(diff) = d["diff"].as_str() {
                            ctx.line(format!(
                                "changes since rev {base} (now rev {}):",
                                field(&d, "current")
                            ));
                            ctx.out.write_str(diff);
                        } else if let Ok(cur) = ctx.api.get(&key, vec![]).await {
                            let now = first_field(&cur, &["current_rev", "rev"]);
                            let q =
                                query([("a", Some(base.to_string())), ("b", Some(now.clone()))]);
                            if let Ok(d) = ctx.api.get(&format!("{key}/diff"), q).await {
                                ctx.line(format!("changes since rev {base} (now rev {now}):"));
                                print_diff(ctx, &d);
                            }
                        }
                    }
                    Err(e)
                }
            }
        }
        WikiCmd::Section {
            page,
            heading,
            body,
            base,
            summary,
        } => {
            let mut b = json!({"heading": heading, "body": ctx.text(body)?});
            set_opt(&mut b, "base", *base);
            set_opt(&mut b, "summary", summary.clone());
            let v = ctx
                .api
                .post(&format!("{}/section", wiki_path(&p, page)), Some(b))
                .await?;
            ctx.emit(&v, print_saved)
        }
        WikiCmd::Append {
            page,
            body,
            summary,
        } => {
            let mut b = json!({"body": ctx.text(body)?});
            set_opt(&mut b, "summary", summary.clone());
            let v = ctx
                .api
                .post(&format!("{}/append", wiki_path(&p, page)), Some(b))
                .await?;
            ctx.emit(&v, print_saved)
        }
        WikiCmd::History { page } => {
            let v = Value::Array(
                ctx.api
                    .list(&format!("{}/revisions", wiki_path(&p, page)), vec![], None)
                    .await?,
            );
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    as_items(v),
                    &[
                        ("REV", "rev"),
                        ("AUTHOR", "author_name"),
                        ("SUMMARY", "summary"),
                        ("AT", "created_at"),
                    ],
                    "no revisions",
                );
            })
        }
        WikiCmd::Diff { page, a, b } => {
            let q = query([("a", Some(a.to_string())), ("b", Some(b.to_string()))]);
            let v = ctx
                .api
                .get(&format!("{}/diff", wiki_path(&p, page)), q)
                .await?;
            ctx.emit(&v, print_diff)
        }
        WikiCmd::Mv { old, new } => {
            let v = ctx
                .api
                .post(
                    &format!("{}/move", wiki_path(&p, old)),
                    Some(json!({"path": new})),
                )
                .await?;
            ctx.emit(&v, |c, _| c.line(format!("moved {old} to {new}")))
        }
        WikiCmd::Rm { page } => {
            ctx.api.delete(&wiki_path(&p, page), vec![]).await?;
            ctx.emit(&json!({"deleted": page}), |c, _| {
                c.line(format!("deleted {page}; the history stays"));
            })
        }
        WikiCmd::Restore { page, rev } => {
            let v = ctx
                .api
                .post(
                    &format!("{}/restore", wiki_path(&p, page)),
                    Some(json!({"rev": rev})),
                )
                .await?;
            ctx.emit(&v, print_saved)
        }
        WikiCmd::Backlinks { page } => {
            let v = ctx
                .api
                .get(&format!("{}/backlinks", wiki_path(&p, page)), vec![])
                .await?;
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), WIKI_COLS, "no backlinks")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_split() {
        assert!(matches!(
            split_pair("card:TER-42", "x"),
            Ok(("card", "TER-42"))
        ));
        assert!(matches!(
            split_pair("path:src/a:b.rs", "x"),
            Ok(("path", "src/a:b.rs"))
        ));
        assert!(split_pair("nocolon", "x").is_err());
        assert!(split_pair(":x", "x").is_err());
    }

    #[test]
    fn wiki_keys_encode() {
        assert_eq!(
            wiki_path("terra", "reports/perf"),
            "/api/wiki/terra%2Freports%2Fperf"
        );
    }
}
