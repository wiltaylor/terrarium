//! The project pages CLI.
use anyhow::Context;
use serde_json::Value;
use crate::{cli::PageCmd, ctx::Ctx, transport::encode, output::field};
use super::{projects::project_path, system::as_items};

pub async fn run(ctx: &Ctx, cmd: &PageCmd) -> anyhow::Result<()> {
    let project = ctx.project_or(None).await?;
    let base = format!("{}/pages", project_path(&project));
    match cmd {
        PageCmd::Ls => {
            let v = ctx.api.get(&base, vec![]).await?;
            ctx.emit(&v, |out, v| {
                for p in as_items(&v["items"]) {
                    out.line(format!("{}  {}  {}", field(p, "slug"), field(p, "position"), field(p, "title")));
                }
            })
        }
        PageCmd::Show { slug } => {
            let v = ctx.api.get(&format!("{base}/{}", encode(slug)), vec![]).await?;
            ctx.emit(&v, |out, v| out.line(serde_json::to_string_pretty(v).unwrap_or_default()))
        }
        PageCmd::Set { slug, from_file } => {
            let text = if from_file == "-" { ctx.text("-")? } else {
                std::fs::read_to_string(from_file).with_context(|| format!("cannot read {from_file}"))?
            };
            let body: Value = serde_json::from_str(&text).context("page input is not JSON")?;
            let v = ctx.api.put(&format!("{base}/{}", encode(slug)), body).await?;
            ctx.emit(&v, |out, _| out.line(format!("saved page {project}/{slug}")))
        }
        PageCmd::Rm { slug } => {
            let v = ctx.api.delete(&format!("{base}/{}", encode(slug)), vec![]).await?;
            ctx.emit(&v, |out, _| out.line(format!("removed page {project}/{slug}")))
        }
    }
}
