//! `terrarium screen <target> ...`: the target display (computer use,
//! docs/api.md §17.10). Not `terrarium agent screen`, which shows an
//! agent's own harness terminal.
//!
//! Each subcommand is one `POST /api/targets/{t}/display/{op}`. A
//! screenshot comes as JSON (`?format=json`, because the agent tunnel
//! carries JSON only), and the CLI writes the PNG to a file and prints its
//! path, so a harness can open the image.

use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use clap::{Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use crate::api::{CliError, query};
use crate::ctx::Ctx;
use crate::transport::encode;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ButtonArg {
    Left,
    Right,
    Middle,
}

/// `record start` or `record stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RecordArg {
    Start,
    Stop,
}

#[derive(Debug, Subcommand)]
pub enum ScreenCmd {
    /// Take a screenshot, write it as a PNG, and print the path
    #[command(visible_alias = "screenshot")]
    Shot {
        /// The PNG file. Default: ./screens/<target>-<time>.png
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Only this part of the screen: x,y,w,h
        #[arg(long)]
        region: Option<String>,
        /// Scale the image down: 0.1 to 1 (for example 0.5)
        #[arg(long)]
        scale: Option<f32>,
    },
    /// Click a mouse button, at X Y when given (needs an exclusive lease)
    Click {
        x: Option<i32>,
        y: Option<i32>,
        #[arg(long, value_enum, default_value = "left")]
        button: ButtonArg,
        /// Click two times
        #[arg(long)]
        double: bool,
    },
    /// Move the mouse pointer to X Y (needs an exclusive lease)
    Move { x: i32, y: i32 },
    /// Press the left button at X1 Y1, move to X2 Y2, and release
    Drag { x1: i32, y1: i32, x2: i32, y2: i32 },
    /// Scroll: `scroll DX DY`, or `scroll X Y DX DY` (a positive DY scrolls down)
    Scroll {
        #[arg(num_args = 2..=4, allow_negative_numbers = true, required = true)]
        values: Vec<i32>,
    },
    /// Press one key chord: ctrl-l, ctrl+alt+delete, Return, shift-f10
    Keys { chord: String },
    /// Type text (`-` reads stdin). Use `keys` for Enter at the end
    Type { text: String },
    /// Read the text on the screen
    Ocr {
        /// Only this part of the screen: x,y,w,h
        #[arg(long)]
        region: Option<String>,
    },
    /// Find a template PNG on the screen; exits 1 when it is not there
    Find {
        png: PathBuf,
        /// The match threshold, 0 to 1
        #[arg(long)]
        threshold: Option<f32>,
        /// Only this part of the screen: x,y,w,h
        #[arg(long)]
        region: Option<String>,
    },
    /// The screen size, the backend, and what it can do
    Info,
    /// Start or stop a recording of the screen (vmlab targets)
    Record {
        #[arg(value_enum)]
        action: RecordArg,
    },
}

fn region(r: &Option<String>) -> anyhow::Result<Option<Value>> {
    r.as_deref()
        .map(|s| {
            terrarium_core::display::Region::parse(s)
                .map(|r| json!(r))
                .map_err(CliError::usage)
        })
        .transpose()
}

/// The default screenshot path: `./screens/<target>-<UTC time>.png`.
pub fn default_shot_path(target: &str, now: chrono::DateTime<chrono::Utc>) -> PathBuf {
    let safe: String = target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    PathBuf::from("screens").join(format!("{safe}-{}.png", now.format("%Y%m%d-%H%M%S%.3f")))
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// The operation name and its body.
fn request(ctx: &Ctx, op: &ScreenCmd) -> anyhow::Result<(&'static str, Value)> {
    let mut b = Map::new();
    let name = match op {
        ScreenCmd::Shot {
            region: r, scale, ..
        } => {
            if let Some(r) = region(r)? {
                b.insert("region".into(), r);
            }
            if let Some(s) = scale {
                b.insert("scale".into(), json!(s));
            }
            "screenshot"
        }
        ScreenCmd::Click {
            x,
            y,
            button,
            double,
        } => {
            match (x, y) {
                (Some(x), Some(y)) => {
                    b.insert("x".into(), json!(x));
                    b.insert("y".into(), json!(y));
                }
                (None, None) => {}
                _ => return Err(CliError::usage("give both X and Y, or neither")),
            }
            if *button != ButtonArg::Left {
                b.insert(
                    "button".into(),
                    json!(match button {
                        ButtonArg::Right => "right",
                        _ => "middle",
                    }),
                );
            }
            if *double {
                b.insert("double".into(), json!(true));
            }
            "click"
        }
        ScreenCmd::Move { x, y } => {
            b.insert("x".into(), json!(x));
            b.insert("y".into(), json!(y));
            "move"
        }
        ScreenCmd::Drag { x1, y1, x2, y2 } => {
            for (k, v) in [("x1", x1), ("y1", y1), ("x2", x2), ("y2", y2)] {
                b.insert(k.into(), json!(v));
            }
            "drag"
        }
        ScreenCmd::Scroll { values } => {
            let (at, d) = match values.as_slice() {
                [dx, dy] => (None, (*dx, *dy)),
                [x, y, dx, dy] => (Some((*x, *y)), (*dx, *dy)),
                _ => return Err(CliError::usage("scroll takes DX DY, or X Y DX DY")),
            };
            if let Some((x, y)) = at {
                b.insert("x".into(), json!(x));
                b.insert("y".into(), json!(y));
            }
            b.insert("dx".into(), json!(d.0));
            b.insert("dy".into(), json!(d.1));
            "scroll"
        }
        ScreenCmd::Keys { chord } => {
            terrarium_core::display::KeyChord::parse(chord).map_err(CliError::usage)?;
            b.insert("chord".into(), json!(chord));
            "keys"
        }
        ScreenCmd::Type { text } => {
            let t = ctx.text(text)?;
            b.insert("text".into(), json!(t));
            "type"
        }
        ScreenCmd::Ocr { region: r } => {
            if let Some(r) = region(r)? {
                b.insert("region".into(), r);
            }
            "ocr"
        }
        ScreenCmd::Find {
            png,
            threshold,
            region: r,
        } => {
            let bytes =
                std::fs::read(png).with_context(|| format!("cannot read {}", png.display()))?;
            b.insert("image_base64".into(), json!(B64.encode(bytes)));
            if let Some(t) = threshold {
                b.insert("threshold".into(), json!(t));
            }
            if let Some(r) = region(r)? {
                b.insert("region".into(), r);
            }
            "find"
        }
        ScreenCmd::Info => "info",
        ScreenCmd::Record { action } => {
            b.insert(
                "action".into(),
                json!(match action {
                    RecordArg::Start => "start",
                    RecordArg::Stop => "stop",
                }),
            );
            "record"
        }
    };
    Ok((name, Value::Object(b)))
}

/// `terrarium screen <target> <op>`.
pub async fn screen(
    ctx: &Ctx,
    target: &str,
    agent: Option<&str>,
    op: &ScreenCmd,
) -> anyhow::Result<()> {
    let (name, mut body) = request(ctx, op)?;
    if let Some(a) = agent {
        body["agent"] = json!(a);
    }
    let path = format!("/api/targets/{}/display/{name}", encode(target));
    let q = query([("format", (name == "screenshot").then(|| "json".to_owned()))]);
    let mut v = ctx
        .api
        .call(crate::transport::Method::Post, &path, q, Some(body))
        .await?;
    match op {
        ScreenCmd::Shot { out, .. } => {
            let png = B64
                .decode(v["png_base64"].as_str().unwrap_or_default())
                .context("the daemon sent no PNG")?;
            let file = out
                .clone()
                .unwrap_or_else(|| default_shot_path(target, chrono::Utc::now()));
            if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            std::fs::write(&file, &png)
                .with_context(|| format!("cannot write {}", file.display()))?;
            let file = absolute(&file);
            if let Some(m) = v.as_object_mut() {
                m.remove("png_base64");
                m.insert("path".into(), json!(file));
            }
            if ctx.json {
                ctx.out.line(serde_json::to_string_pretty(&v)?);
            } else {
                ctx.out.line(format!(
                    "{} ({}x{}, screen {}x{})",
                    file.display(),
                    v["width"],
                    v["height"],
                    v["screen"]["width"],
                    v["screen"]["height"]
                ));
            }
            Ok(())
        }
        _ if ctx.json => {
            ctx.out.line(serde_json::to_string_pretty(&v)?);
            if matches!(op, ScreenCmd::Find { .. }) && v["found"] == false {
                return Err(CliError::RemoteExit(1).into());
            }
            Ok(())
        }
        ScreenCmd::Info => {
            let ops: Vec<&str> = v["ops"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            ctx.out.line(format!(
                "target:  {} ({}, backend {})",
                v["target"].as_str().unwrap_or(target),
                v["kind"].as_str().unwrap_or("?"),
                v["backend"].as_str().unwrap_or("?")
            ));
            ctx.out
                .line(format!("screen:  {}x{}", v["width"], v["height"]));
            ctx.out.line(format!("ops:     {}", ops.join(" ")));
            ctx.out
                .line(format!("ocr:     {}", v["ocr"].as_str().unwrap_or("none")));
            let input = if v["input"]["allowed"] == true {
                "allowed".to_owned()
            } else {
                format!(
                    "not allowed: {}",
                    v["input"]["reason"].as_str().unwrap_or("no reason")
                )
            };
            ctx.out.line(format!("input:   {input}"));
            Ok(())
        }
        ScreenCmd::Ocr { .. } => {
            ctx.out.line(v["text"].as_str().unwrap_or_default());
            Ok(())
        }
        ScreenCmd::Find { .. } => {
            if v["found"] != true {
                ctx.out.line("no match");
                return Err(CliError::RemoteExit(1).into());
            }
            ctx.out.line(format!(
                "found at x={} y={} w={} h={} score={} center={},{}",
                v["x"], v["y"], v["w"], v["h"], v["score"], v["center"][0], v["center"][1]
            ));
            Ok(())
        }
        ScreenCmd::Record { .. } => {
            let out = v["output"].as_str().unwrap_or_default();
            ctx.out.line(if out.is_empty() {
                format!("recording: {}", v["recording"])
            } else {
                out.to_owned()
            });
            Ok(())
        }
        _ => {
            let detail = match op {
                ScreenCmd::Keys { .. } => format!(" {}", v["chord"].as_str().unwrap_or("")),
                ScreenCmd::Type { .. } => format!(" {} characters", v["chars"]),
                _ => String::new(),
            };
            ctx.out.line(format!("{name} ok{detail}"));
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};
    use crate::ctx::AgentEnv;
    use crate::transport::Method;
    use crate::transport::mock::MockTransport;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("terrarium").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    async fn run(args: &[&str], reply: Value) -> (anyhow::Result<()>, String, MockTransport) {
        let cli = parse(args);
        let mock = MockTransport::new();
        mock.reply(200, reply);
        let env = AgentEnv {
            agent: "01AGENTAGENTAGENTAGENTAGEN".into(),
            socket: "/nonexistent.sock".into(),
        };
        let mut ctx = Ctx::for_test(Arc::new(mock.clone()), Some(env));
        ctx.json = cli.json;
        let r = crate::commands::run(&ctx, &cli.command).await;
        (r, ctx.out.captured(), mock)
    }

    #[test]
    fn the_command_parses() {
        let c = parse(&["screen", "box", "scroll", "-3", "2"]);
        let Command::Screen { target, op, .. } = c.command else {
            panic!()
        };
        assert_eq!(target, "box");
        assert!(matches!(op, ScreenCmd::Scroll { ref values } if values == &[-3, 2]));
        assert!(Cli::try_parse_from(["terrarium", "screen", "box", "scroll", "1"]).is_err());
        let c = parse(&[
            "screen", "box", "--agent", "w1", "click", "10", "20", "--double",
        ]);
        assert!(matches!(c.command, Command::Screen { agent: Some(ref a), .. } if a == "w1"));
        let c = parse(&["screen", "box", "shot", "--agent", "w1"]);
        assert!(matches!(c.command, Command::Screen { agent: Some(_), .. }));
    }

    #[test]
    fn the_default_path_is_under_screens() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-10-27T09:08:07.123Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            default_shot_path("terra-win", t),
            PathBuf::from("screens/terra-win-20261027-090807.123.png")
        );
        assert_eq!(
            default_shot_path("a/b", t),
            PathBuf::from("screens/a_b-20261027-090807.123.png")
        );
    }

    #[tokio::test]
    async fn shot_writes_the_png_and_prints_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("s.png");
        let png = b"\x89PNG fake".to_vec();
        let (r, text, mock) = run(
            &[
                "screen",
                "box",
                "shot",
                "--out",
                out.to_str().unwrap(),
                "--scale",
                "0.5",
                "--region",
                "0,0,100,50",
            ],
            json!({"target": "box", "op": "screenshot", "width": 50, "height": 25,
                   "screen": {"width": 1280, "height": 800}, "png_base64": B64.encode(&png)}),
        )
        .await;
        r.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), png);
        assert_eq!(
            text.trim(),
            format!("{} (50x25, screen 1280x800)", out.display())
        );
        let reqs = mock.requests();
        assert_eq!(reqs[0].method, Method::Post);
        assert_eq!(reqs[0].path, "/api/targets/box/display/screenshot");
        assert_eq!(
            reqs[0].query,
            vec![("format".to_owned(), "json".to_owned())]
        );
        assert_eq!(
            reqs[0].body,
            Some(json!({"scale": 0.5, "region": {"x": 0, "y": 0, "w": 100, "h": 50}}))
        );
    }

    #[tokio::test]
    async fn input_ops_send_their_fields() {
        for (args, path, body) in [
            (
                vec!["screen", "box", "click", "10", "20", "--button", "right"],
                "/api/targets/box/display/click",
                json!({"x": 10, "y": 20, "button": "right"}),
            ),
            (
                vec!["screen", "box", "scroll", "5", "6", "0", "-3"],
                "/api/targets/box/display/scroll",
                json!({"x": 5, "y": 6, "dx": 0, "dy": -3}),
            ),
            (
                vec!["screen", "box", "keys", "ctrl+l"],
                "/api/targets/box/display/keys",
                json!({"chord": "ctrl+l"}),
            ),
            (
                vec!["screen", "box", "--agent", "w1", "drag", "1", "2", "3", "4"],
                "/api/targets/box/display/drag",
                json!({"x1": 1, "y1": 2, "x2": 3, "y2": 4, "agent": "w1"}),
            ),
            (
                vec!["screen", "box", "record", "start"],
                "/api/targets/box/display/record",
                json!({"action": "start"}),
            ),
        ] {
            let (r, _, mock) = run(&args, json!({"ok": true, "chord": "ctrl-l"})).await;
            r.unwrap();
            let reqs = mock.requests();
            assert_eq!(reqs[0].path, path, "{args:?}");
            assert_eq!(reqs[0].body, Some(body), "{args:?}");
        }
        // A bad chord never reaches the daemon.
        let (r, _, mock) = run(&["screen", "box", "keys", "ctrl-flurb"], json!({})).await;
        assert!(r.is_err());
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn find_exits_1_when_there_is_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let tpl = dir.path().join("ok.png");
        std::fs::write(&tpl, b"png").unwrap();
        let (r, text, _) = run(
            &["screen", "box", "find", tpl.to_str().unwrap()],
            json!({"found": false}),
        )
        .await;
        assert_eq!(crate::api::exit_code_of(&r.unwrap_err()), 1);
        assert_eq!(text.trim(), "no match");
        let (r, text, _) = run(
            &["screen", "box", "find", tpl.to_str().unwrap()],
            json!({"found": true, "x": 1, "y": 2, "w": 3, "h": 4, "score": 0.9, "center": [2, 4]}),
        )
        .await;
        r.unwrap();
        assert!(text.contains("center=2,4"), "{text}");
    }
}
