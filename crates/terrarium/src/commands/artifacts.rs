//! `artifact`: PLAN §15.2 and the §17 routes. Versions are card labels
//! (TER-58): an artifact links to a label such as `version:1.4.0`.

use std::path::Path;

use anyhow::Context;
use serde_json::{Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::ArtifactCmd;
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

/// The largest artifact the CLI sends.
pub const MAX_ARTIFACT: usize = 32 * 1024 * 1024;

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A content type from the file extension. The daemon sniffs and fixes it.
pub fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "md" | "markdown" => "text/markdown",
        "txt" | "log" => "text/plain",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "csv" => "text/csv",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

/// A directory as a tar archive, made by the system `tar`.
fn tar_dir(dir: &Path) -> anyhow::Result<Vec<u8>> {
    let out = std::process::Command::new("tar")
        .arg("-C")
        .arg(dir)
        .args(["-cf", "-", "."])
        .output()
        .context("cannot run `tar` to bundle the directory")?;
    if !out.status.success() {
        anyhow::bail!(
            "`tar` failed on {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// The links of a push, in the order page, labels, card.
pub fn attaches_of(
    page: &Option<String>,
    labels: &[String],
    card: &Option<String>,
) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = page.iter().map(|p| ("page", p.clone())).collect();
    out.extend(labels.iter().map(|l| ("label", l.clone())));
    out.extend(card.iter().map(|c| ("card", c.clone())));
    out
}

/// The artifact has the label `label` among its links.
pub fn has_label(a: &Value, label: &str) -> bool {
    as_items(&a["links"])
        .iter()
        .any(|l| l["kind"] == "label" && l["ref"].as_str() == Some(label.trim()))
}

/// The Markdown that embeds the artifact in a wiki page (PLAN §15.1): an
/// image for an image, a fenced `artifact` block for a bundle or an HTML
/// file, else a link.
pub fn embed_of(a: &Value) -> String {
    let id = field(a, "id");
    let name = field(a, "name");
    let ctype = field(a, "content_type");
    if field(a, "kind") == "bundle" || ctype.starts_with("text/html") {
        format!("```artifact {id}\n```")
    } else if ctype.starts_with("image/") {
        format!("![{name}](artifact:{id})")
    } else {
        format!("[{name}](artifact:{id})")
    }
}

fn mib(n: usize) -> String {
    format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
}

/// The body of `POST /api/projects/{name}/artifacts`.
pub fn push_body(
    path: &Path,
    name: Option<&str>,
    attach: Option<(&str, String)>,
) -> anyhow::Result<Value> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "artifact".into());
    let (data, kind, ctype) = if meta.is_dir() {
        (tar_dir(path)?, "bundle", "application/x-tar")
    } else {
        if meta.len() as usize > MAX_ARTIFACT {
            return Err(CliError::usage(format!(
                "{} is {}; artifacts are capped at 32 MiB",
                path.display(),
                mib(meta.len() as usize)
            )));
        }
        let data =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        (data, "file", content_type(path))
    };
    if data.len() > MAX_ARTIFACT {
        return Err(CliError::usage(format!(
            "{} is {} as a bundle; artifacts are capped at 32 MiB",
            path.display(),
            mib(data.len())
        )));
    }
    let mut b = json!({
        "name": name.map_or(base, str::to_owned),
        "content_type": ctype,
        "kind": kind,
        "data_base64": base64(&data),
    });
    if let Some((k, r)) = attach {
        b["attach"] = json!({"kind": k, "ref": r});
    }
    Ok(b)
}

pub async fn artifact(ctx: &Ctx, cmd: &ArtifactCmd) -> anyhow::Result<()> {
    match cmd {
        ArtifactCmd::Push {
            path,
            name,
            page,
            labels,
            card,
            note,
        } => {
            let p = ctx.project_or(None).await?;
            if note.is_some() {
                eprintln!(
                    "terrarium: the daemon does not store artifact notes yet; --note is ignored"
                );
            }
            // The upload takes the first link; the others follow it.
            let mut links = attaches_of(page, labels, card).into_iter();
            let body = push_body(path, name.as_deref(), links.next())?;
            let mut a = ctx
                .api
                .post_upload(&format!("{}/artifacts", project_path(&p)), body)
                .await?;
            let key = a["key"]
                .as_str()
                .or(a["id"].as_str())
                .unwrap_or_default()
                .to_owned();
            for (kind, r) in links {
                let linked = ctx
                    .api
                    .post(
                        &format!("/api/artifacts/{}/links", encode(&key)),
                        Some(json!({"kind": kind, "ref": r})),
                    )
                    .await?;
                a["links"] = linked["links"].clone();
            }
            if a.get("url").is_none()
                && let Some(id) = a["id"].as_str()
                && let Ok(u) = mint_url(ctx, id).await
            {
                a["url"] = u["url"].clone();
            }
            ctx.emit(&a, |c, a| {
                c.line(format!(
                    "pushed {} {} ({})",
                    field(a, "id"),
                    field(a, "name"),
                    field(a, "kind")
                ));
                c.line("Embed it in a page with:");
                c.line(embed_of(a));
                if let Some(u) = a["url"].as_str() {
                    c.line(format!(
                        "Preview (expires in 5 minutes; never paste it into a page): {u}"
                    ));
                }
            })
        }
        ArtifactCmd::Ls { page, label, card } => {
            let p = ctx.project_or(None).await?;
            let q = query([
                ("page", page.clone()),
                ("card", card.clone()),
                ("label", label.clone()),
            ]);
            let mut items = ctx
                .api
                .list(&format!("{}/artifacts", project_path(&p)), q, None)
                .await?;
            // The daemon filters by `label` (TER-129). A daemon from before
            // ignores it, so keep the labelled ones here too.
            if let Some(l) = label {
                items.retain(|a| has_label(a, l));
            }
            let v = Value::Array(items);
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    as_items(v),
                    &[
                        ("ID", "id"),
                        ("NAME", "name"),
                        ("KIND", "kind"),
                        ("TYPE", "content_type"),
                        ("SIZE", "size"),
                        ("CREATED", "created_at"),
                    ],
                    "no artifacts",
                );
            })
        }
        ArtifactCmd::Url { id } => {
            let u = mint_url(ctx, id).await?;
            ctx.emit(&u, |c, u| c.line(field(u, "url")))
        }
        ArtifactCmd::Rm { id } => {
            ctx.api
                .delete(&format!("/api/artifacts/{}", encode(id)), vec![])
                .await?;
            ctx.emit(&json!({"deleted": id}), |c, _| {
                c.line(format!("deleted {id}"));
            })
        }
    }
}

/// A signed preview URL: `POST /api/artifacts/{id}/url`.
async fn mint_url(ctx: &Ctx, id: &str) -> anyhow::Result<Value> {
    // Short artifact IDs are project-local. Qualified keys and ULIDs already
    // identify their project, and the daemon checks read access to that project.
    let key = match ctx.project.as_deref() {
        Some(project) if id.starts_with("ART-") => format!("{project}/{id}"),
        _ => id.to_owned(),
    };
    ctx.api
        .post(&format!("/api/artifacts/{}/url", encode(&key)), None)
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe]), "//4=");
    }

    #[test]
    fn content_types_guess() {
        assert_eq!(content_type(Path::new("a/report.HTML")), "text/html");
        assert_eq!(content_type(Path::new("shot.png")), "image/png");
        assert_eq!(content_type(Path::new("bin")), "application/octet-stream");
    }

    #[test]
    fn push_links_to_every_target_given() {
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            attaches_of(
                &s("plans/x"),
                &["version:1.0.0".to_owned(), "version:vnext".to_owned()],
                &s("DEM-1")
            ),
            vec![
                ("page", "plans/x".to_owned()),
                ("label", "version:1.0.0".to_owned()),
                ("label", "version:vnext".to_owned()),
                ("card", "DEM-1".to_owned())
            ]
        );
        assert_eq!(
            attaches_of(&None, &[], &s("DEM-1")),
            vec![("card", "DEM-1".to_owned())]
        );
        assert!(attaches_of(&None, &[], &None).is_empty());
    }

    #[test]
    fn label_filter_reads_the_links() {
        let a = json!({"links": [{"kind": "card", "ref": "TER-1"},
                                 {"kind": "label", "ref": "version:1.0.0"}]});
        assert!(has_label(&a, "version:1.0.0"));
        assert!(!has_label(&a, "version:vnext"));
        assert!(!has_label(&a, "TER-1"));
    }

    #[test]
    fn embed_syntax_follows_the_kind() {
        let a = |kind: &str, ct: &str| json!({"id": "ART-3", "name": "x.png", "kind": kind, "content_type": ct});
        assert_eq!(
            embed_of(&a("file", "image/png")),
            "![x.png](artifact:ART-3)"
        );
        assert_eq!(
            embed_of(&a("file", "text/markdown")),
            "[x.png](artifact:ART-3)"
        );
        assert_eq!(embed_of(&a("file", "text/html")), "```artifact ART-3\n```");
        assert_eq!(
            embed_of(&a("bundle", "application/x-tar")),
            "```artifact ART-3\n```"
        );
    }

    #[test]
    fn a_directory_becomes_a_bundle() {
        let dir = std::env::temp_dir().join(format!("terrarium-art-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("site")).unwrap();
        std::fs::write(dir.join("site/index.html"), "<h1>hi</h1>").unwrap();
        let b = push_body(&dir.join("site"), None, Some(("page", "reports/x".into()))).unwrap();
        assert_eq!(b["kind"], "bundle");
        assert_eq!(b["name"], "site");
        assert_eq!(b["content_type"], "application/x-tar");
        assert_eq!(b["attach"], json!({"kind": "page", "ref": "reports/x"}));
        assert!(b["data_base64"].as_str().unwrap().len() > 100);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let f = push_body(&dir.join("a.txt"), Some("n.txt"), None).unwrap();
        assert_eq!(f["kind"], "file");
        assert_eq!(f["name"], "n.txt");
        assert_eq!(f["data_base64"], "aGVsbG8=");
        assert!(f.get("note").is_none() && f.get("size").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
