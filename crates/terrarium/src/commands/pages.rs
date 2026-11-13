//! The project pages CLI.
use super::{projects::project_path, system::as_items};
use crate::{cli::PageCmd, ctx::Ctx, output::field, transport::encode};
use anyhow::Context;
use serde_json::Value;

pub async fn run(ctx: &Ctx, cmd: &PageCmd) -> anyhow::Result<()> {
    let project = ctx.project_or(None).await?;
    let base = format!("{}/pages", project_path(&project));
    let layout_path = format!("{}/page-layout", project_path(&project));
    match cmd {
        PageCmd::Layout => {
            let v = ctx.api.get(&layout_path, vec![]).await?;
            ctx.emit(&v, |out, v| out.line(serde_json::to_string_pretty(v).unwrap_or_default()))
        }
        PageCmd::SaveLayout { from_file } => {
            let text = if from_file == "-" { ctx.text("-")? } else {
                std::fs::read_to_string(from_file).with_context(|| format!("cannot read {from_file}"))?
            };
            let body: Value = serde_json::from_str(&text).context("layout input is not JSON")?;
            let v = ctx.api.put(&layout_path, body).await?;
            ctx.emit(&v, |out, v| out.line(serde_json::to_string_pretty(v).unwrap_or_default()))
        }
        PageCmd::Reset { builtin, base_revision } => {
            let v = ctx.api.post(&format!("{layout_path}/reset"), serde_json::json!({
                "builtin": builtin, "base_revision": base_revision
            })).await?;
            ctx.emit(&v, |out, v| out.line(serde_json::to_string_pretty(v).unwrap_or_default()))
        }
        PageCmd::Copy { source_project, base_revision, confirm } => {
            anyhow::ensure!(*confirm, "copy requires --confirm");
            let v = ctx.api.post(&format!("{layout_path}/copy"), serde_json::json!({
                "source_project": source_project, "base_revision": base_revision, "confirm": true
            })).await?;
            ctx.emit(&v, |out, v| out.line(serde_json::to_string_pretty(v).unwrap_or_default()))
        }
        PageCmd::Ls => {
            let v = ctx.api.get(&base, vec![]).await?;
            ctx.emit(&v, |out, v| {
                for p in as_items(&v["items"]) {
                    out.line(format!(
                        "{}  {}  {}",
                        field(p, "slug"),
                        field(p, "position"),
                        field(p, "title")
                    ));
                }
            })
        }
        PageCmd::Show { slug } => {
            let v = ctx
                .api
                .get(&format!("{base}/{}", encode(slug)), vec![])
                .await?;
            ctx.emit(&v, |out, v| {
                out.line(serde_json::to_string_pretty(v).unwrap_or_default())
            })
        }
        PageCmd::Set { slug, from_file } => {
            let text = if from_file == "-" {
                ctx.text("-")?
            } else {
                std::fs::read_to_string(from_file)
                    .with_context(|| format!("cannot read {from_file}"))?
            };
            let body: Value = serde_json::from_str(&text).context("page input is not JSON")?;
            let v = ctx
                .api
                .put(&format!("{base}/{}", encode(slug)), body)
                .await?;
            ctx.emit(&v, |out, _| {
                out.line(format!("saved page {project}/{slug}"))
            })
        }
        PageCmd::Rm { slug } => {
            let v = ctx
                .api
                .delete(&format!("{base}/{}", encode(slug)), vec![])
                .await?;
            ctx.emit(&v, |out, _| {
                out.line(format!("removed page {project}/{slug}"))
            })
        }
    }
}
