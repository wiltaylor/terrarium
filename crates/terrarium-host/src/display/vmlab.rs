//! The vmlab display driver: `vmlab vm screenshot|mouse-move|click|drag|
//! sendkeys|ocr|find-image` and `vmlab record start|stop`, run in the lab
//! directory with the same lab and path rules as the other vmlab verbs
//! ([`crate::vmlab::lab_dir`]).
//!
//! Limits of vmlab 0.34 (docs/api.md §17.10):
//!
//! - No text input on the CLI: `type` sends one `sendkeys` chord for each
//!   character. vmlab chords take letters, digits, and named keys only, so
//!   `type` covers letters, digits, space, Tab, newline, and the shifted
//!   digits `!@#$%^&*()`. Other characters are `bad_request`.
//! - No scroll, and `drag` uses the left button only.
//! - A double click is two clicks.
//! - The screenshot and a `find` template go through a file in
//!   `<lab>/.terrarium-stage/`, because vmlab writes and reads files by path.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use terrarium_core::display::{Button, KeyChord, RecordAction, Region, us_char_chord};

use super::runner::{Cmd, CmdOut, Runner};
use super::{DResult, DisplayCtx, DisplayDriver, DisplayError, Features};
use crate::verb::ident;

/// The most characters that `type` sends to a vmlab machine at a time:
/// each one is a `vmlab vm sendkeys` process.
pub const MAX_VMLAB_TYPE: usize = 500;

/// The staging dir in the lab dir, as for `vmlab.cp`.
const STAGE: &str = ".terrarium-stage";

pub struct VmlabDriver {
    runner: Arc<dyn Runner>,
    bin: String,
    dir: PathBuf,
    machine: String,
    timeout: Duration,
}

impl VmlabDriver {
    pub fn new(ctx: &DisplayCtx, lab: &Path, machine: &str) -> DResult<Self> {
        let dir = crate::vmlab::lab_dir(lab, &ctx.policy)
            .map_err(|e| DisplayError::BadRequest(e.message))?;
        let machine = ident("machine", machine)
            .map_err(|e| DisplayError::BadRequest(e.message))?
            .to_owned();
        Ok(Self {
            runner: ctx.runner.clone(),
            bin: ctx.vmlab_bin.clone(),
            dir,
            machine,
            timeout: ctx.timeout,
        })
    }

    fn s(v: impl ToString) -> String {
        v.to_string()
    }

    async fn vmlab(&self, what: &str, args: Vec<String>) -> DResult<CmdOut> {
        let mut cmd = Cmd::new(self.bin.clone(), args);
        cmd.cwd = Some(self.dir.clone());
        cmd.timeout = self.timeout;
        let out = self
            .runner
            .run(cmd)
            .await
            .map_err(|e| DisplayError::Failed(format!("vmlab {what}: {e}")))?;
        if out.success() {
            return Ok(out);
        }
        Err(self.failure(what, &out))
    }

    fn failure(&self, what: &str, out: &CmdOut) -> DisplayError {
        let m = &self.machine;
        let tail = out.stderr_tail();
        match out.code {
            Some(4) => DisplayError::Failed(format!("vmlab has no machine `{m}` in the lab")),
            Some(6) => DisplayError::Unsupported(format!(
                "machine `{m}` has no display (vmlab exit 6: a container has no screen)"
            )),
            Some(2) => DisplayError::BadRequest(format!("vmlab {what}: {tail}")),
            c => DisplayError::Failed(format!(
                "vmlab {what} failed (exit {}): {tail}",
                c.map_or("-".into(), |c| c.to_string())
            )),
        }
    }

    /// A file in the lab's staging dir. Removed on drop.
    fn stage_file(&self, suffix: &str) -> DResult<tempfile::NamedTempFile> {
        let dir = self.dir.join(STAGE);
        std::fs::create_dir_all(&dir)
            .map_err(|e| DisplayError::Failed(format!("create {}: {e}", dir.display())))?;
        tempfile::Builder::new()
            .prefix("display-")
            .suffix(suffix)
            .tempfile_in(&dir)
            .map_err(|e| DisplayError::Failed(format!("a file in {}: {e}", dir.display())))
    }

    fn path_arg(p: &Path) -> DResult<String> {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| DisplayError::Failed("the staging path is not UTF-8".into()))
    }

    fn region_args(a: &mut Vec<String>, region: Option<Region>) {
        if let Some(r) = region {
            a.extend([
                Self::s("--region"),
                Self::s(r.x),
                Self::s(r.y),
                Self::s(r.w),
                Self::s(r.h),
            ]);
        }
    }

    async fn click_once(&self, at: Option<(i32, i32)>, button: Button) -> DResult<()> {
        let mut a = vec![Self::s("vm"), Self::s("click")];
        if button != Button::Left {
            a.extend([Self::s("--button"), Self::s(button.as_str())]);
        }
        a.push(self.machine.clone());
        if let Some((x, y)) = at {
            a.extend([Self::s(x), Self::s(y)]);
        }
        self.vmlab("vm click", a).await.map(|_| ())
    }

    async fn chord(&self, chord: &str) -> DResult<()> {
        self.vmlab(
            "vm sendkeys",
            vec![
                Self::s("vm"),
                Self::s("sendkeys"),
                self.machine.clone(),
                Self::s(chord),
            ],
        )
        .await
        .map(|_| ())
    }
}

/// Parse `x=.. y=.. w=.. h=.. score=.. center=x,y` of `vmlab vm find-image`.
pub fn parse_find(stdout: &str) -> Value {
    let mut v = json!({"found": true});
    for kv in stdout.split_whitespace() {
        let Some((k, val)) = kv.split_once('=') else {
            continue;
        };
        let parsed = if k == "center" {
            let mut it = val.split(',').filter_map(|n| n.parse::<i64>().ok());
            match (it.next(), it.next()) {
                (Some(x), Some(y)) => json!([x, y]),
                _ => continue,
            }
        } else if let Ok(n) = val.parse::<i64>() {
            json!(n)
        } else if let Ok(f) = val.parse::<f64>() {
            json!(f)
        } else {
            json!(val)
        };
        v[k] = parsed;
    }
    v
}

impl DisplayDriver for VmlabDriver {
    fn backend(&self) -> &'static str {
        "vmlab"
    }

    fn features(&self) -> Features {
        Features {
            scroll: false,
            ocr: true,
            find: true,
            record: true,
        }
    }

    async fn screenshot(&self) -> DResult<Vec<u8>> {
        let file = self.stage_file(".png")?;
        let path = Self::path_arg(file.path())?;
        self.vmlab(
            "vm screenshot",
            vec![
                Self::s("vm"),
                Self::s("screenshot"),
                self.machine.clone(),
                path,
            ],
        )
        .await?;
        tokio::fs::read(file.path())
            .await
            .map_err(|e| DisplayError::Failed(format!("read the vmlab screenshot: {e}")))
    }

    async fn move_to(&self, x: i32, y: i32) -> DResult<()> {
        self.vmlab(
            "vm mouse-move",
            vec![
                Self::s("vm"),
                Self::s("mouse-move"),
                self.machine.clone(),
                Self::s(x),
                Self::s(y),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn click(&self, at: Option<(i32, i32)>, button: Button, double: bool) -> DResult<()> {
        self.click_once(at, button).await?;
        if double {
            self.click_once(None, button).await?;
        }
        Ok(())
    }

    async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> DResult<()> {
        self.vmlab(
            "vm drag",
            vec![
                Self::s("vm"),
                Self::s("drag"),
                self.machine.clone(),
                Self::s(from.0),
                Self::s(from.1),
                Self::s(to.0),
                Self::s(to.1),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn scroll(&self, _at: Option<(i32, i32)>, _dx: i32, _dy: i32) -> DResult<()> {
        Err(DisplayError::Unsupported(
            "vmlab has no scroll input; use keys pagedown or pageup".into(),
        ))
    }

    async fn keys(&self, chord: &KeyChord) -> DResult<()> {
        let c = chord.vmlab().map_err(DisplayError::BadRequest)?;
        self.chord(&c).await
    }

    async fn type_text(&self, text: &str) -> DResult<()> {
        if text.chars().count() > MAX_VMLAB_TYPE {
            return Err(DisplayError::BadRequest(format!(
                "a vmlab target types at most {MAX_VMLAB_TYPE} characters at a time"
            )));
        }
        let mut chords = Vec::new();
        for c in text.chars() {
            let chord = us_char_chord(c)
                .and_then(|k| k.vmlab().ok())
                .ok_or_else(|| {
                    DisplayError::BadRequest(format!(
                        "vmlab cannot type {c:?}: its chords take letters, digits, space, tab, \
                         newline, and !@#$%^&*() only (US layout)"
                    ))
                })?;
            chords.push(chord);
        }
        for c in chords {
            self.chord(&c).await?;
        }
        Ok(())
    }

    async fn ocr(&self, region: Option<Region>) -> DResult<String> {
        let mut a = vec![Self::s("vm"), Self::s("ocr")];
        Self::region_args(&mut a, region);
        a.push(self.machine.clone());
        let out = self.vmlab("vm ocr", a).await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    }

    async fn find(
        &self,
        template: &[u8],
        threshold: Option<f32>,
        region: Option<Region>,
    ) -> DResult<Value> {
        let file = self.stage_file(".png")?;
        std::fs::write(file.path(), template)
            .map_err(|e| DisplayError::Failed(format!("write the template: {e}")))?;
        let mut a = vec![Self::s("vm"), Self::s("find-image")];
        if let Some(t) = threshold {
            a.extend([Self::s("--threshold"), Self::s(t)]);
        }
        Self::region_args(&mut a, region);
        a.push(self.machine.clone());
        a.push(Self::path_arg(file.path())?);
        let mut cmd = Cmd::new(self.bin.clone(), a);
        cmd.cwd = Some(self.dir.clone());
        cmd.timeout = self.timeout;
        let out = self
            .runner
            .run(cmd)
            .await
            .map_err(|e| DisplayError::Failed(format!("vmlab vm find-image: {e}")))?;
        if out.success() {
            return Ok(parse_find(&String::from_utf8_lossy(&out.stdout)));
        }
        if out.code == Some(1) && out.stderr_tail().contains("no match") {
            return Ok(json!({"found": false}));
        }
        Err(self.failure("vm find-image", &out))
    }

    async fn record(&self, action: RecordAction) -> DResult<Value> {
        let verb = match action {
            RecordAction::Start => "start",
            RecordAction::Stop => "stop",
        };
        let out = self
            .vmlab(
                "record",
                vec![Self::s("record"), Self::s(verb), self.machine.clone()],
            )
            .await?;
        Ok(json!({"recording": action == RecordAction::Start,
                  "output": String::from_utf8_lossy(&out.stdout).trim()}))
    }
}

#[cfg(test)]
mod tests {
    use super::super::runner::FakeRunner;
    use super::super::testing::{ctx, png};
    use super::super::{dispatch, image};
    use super::*;
    use crate::config::PathPolicy;
    use crate::protocol::{DisplayTarget, ErrorCode};
    use terrarium_core::display::DisplayOp;

    fn target(lab: &Path) -> DisplayTarget {
        DisplayTarget::Vmlab {
            lab: lab.to_path_buf(),
            machine: "win11".into(),
        }
    }

    async fn lines_of(op: DisplayOp) -> Vec<Vec<String>> {
        let lab = tempfile::tempdir().unwrap();
        let fake = FakeRunner::ok();
        let c = ctx(fake.clone());
        dispatch(&c, &target(lab.path()), &op).await.unwrap();
        for cmd in fake.cmds() {
            assert_eq!(cmd.cwd.as_deref(), Some(lab.path()), "runs in the lab dir");
        }
        fake.lines()
    }

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    #[tokio::test]
    async fn input_ops_build_vmlab_command_lines() {
        assert_eq!(
            lines_of(DisplayOp::Move { x: 10, y: 20 }).await,
            [v(&["vmlab", "vm", "mouse-move", "win11", "10", "20"])]
        );
        assert_eq!(
            lines_of(DisplayOp::Click {
                x: Some(5),
                y: Some(6),
                button: Button::Right,
                double: true
            })
            .await,
            [
                v(&[
                    "vmlab", "vm", "click", "--button", "right", "win11", "5", "6"
                ]),
                v(&["vmlab", "vm", "click", "--button", "right", "win11"]),
            ]
        );
        assert_eq!(
            lines_of(DisplayOp::Drag {
                x1: 1,
                y1: 2,
                x2: 3,
                y2: 4
            })
            .await,
            [v(&["vmlab", "vm", "drag", "win11", "1", "2", "3", "4"])]
        );
        assert_eq!(
            lines_of(DisplayOp::Keys {
                chord: "ctrl+alt+Delete".into()
            })
            .await,
            [v(&["vmlab", "vm", "sendkeys", "win11", "ctrl-alt-delete"])]
        );
        assert_eq!(
            lines_of(DisplayOp::Type {
                text: "Hi 1!\n".into()
            })
            .await,
            [
                v(&["vmlab", "vm", "sendkeys", "win11", "shift-h"]),
                v(&["vmlab", "vm", "sendkeys", "win11", "i"]),
                v(&["vmlab", "vm", "sendkeys", "win11", "space"]),
                v(&["vmlab", "vm", "sendkeys", "win11", "1"]),
                v(&["vmlab", "vm", "sendkeys", "win11", "shift-1"]),
                v(&["vmlab", "vm", "sendkeys", "win11", "enter"]),
            ]
        );
        assert_eq!(
            lines_of(DisplayOp::Record {
                action: RecordAction::Start
            })
            .await,
            [v(&["vmlab", "record", "start", "win11"])]
        );
    }

    #[tokio::test]
    async fn refusals_run_nothing() {
        let lab = tempfile::tempdir().unwrap();
        let fake = FakeRunner::ok();
        let c = ctx(fake.clone());
        let t = target(lab.path());
        for (op, code) in [
            (
                DisplayOp::Type { text: "a.b".into() },
                ErrorCode::BadRequest,
            ),
            (
                DisplayOp::Keys {
                    chord: "ctrl-period".into(),
                },
                ErrorCode::BadRequest,
            ),
            (
                DisplayOp::Scroll {
                    x: None,
                    y: None,
                    dx: 0,
                    dy: 3,
                },
                ErrorCode::Unsupported,
            ),
        ] {
            let e = dispatch(&c, &t, &op).await.unwrap_err();
            assert_eq!(e.code(), code, "{op:?}: {e}");
        }
        // A machine that looks like a flag, and a lab outside the roots.
        let bad = DisplayTarget::Vmlab {
            lab: lab.path().to_path_buf(),
            machine: "--all".into(),
        };
        let e = dispatch(&c, &bad, &DisplayOp::Info).await.unwrap_err();
        assert_eq!(e.code(), ErrorCode::BadRequest);
        let mut walled = ctx(fake.clone());
        walled.policy = PathPolicy {
            roots: Some(vec!["/nonexistent-root".into()]),
        };
        assert!(dispatch(&walled, &t, &DisplayOp::Info).await.is_err());
        assert!(
            dispatch(
                &DisplayCtx {
                    vmlab_ok: false,
                    ..c.clone()
                },
                &t,
                &DisplayOp::Info
            )
            .await
            .is_err()
        );
        assert!(fake.lines().is_empty(), "{:?}", fake.lines());
    }

    #[tokio::test]
    async fn screenshot_reads_the_file_vmlab_wrote() {
        let lab = tempfile::tempdir().unwrap();
        let shot = png(16, 8);
        let s2 = shot.clone();
        let fake = FakeRunner::new(move |c| {
            assert_eq!(&c.args[..3], ["vm", "screenshot", "win11"]);
            let path = c.args[3].clone();
            assert!(path.contains(".terrarium-stage/display-"), "{path}");
            std::fs::write(&path, &s2).unwrap();
            Ok(CmdOut::ok(path.into_bytes()))
        });
        let c = ctx(fake);
        let out = dispatch(
            &c,
            &target(lab.path()),
            &DisplayOp::Screenshot {
                region: None,
                scale: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(out.png.as_deref(), Some(shot.as_slice()));
        assert_eq!(out.value["width"], 16);
        // The staging file is gone.
        let left: Vec<_> = std::fs::read_dir(lab.path().join(STAGE)).unwrap().collect();
        assert!(left.is_empty());
        let info = dispatch(&c, &target(lab.path()), &DisplayOp::Info)
            .await
            .unwrap();
        assert_eq!(info.value["ocr"], "vmlab");
        assert!(
            info.value["ops"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o == "record")
        );
        assert_eq!(image::dims(&shot).unwrap(), (16, 8));
    }

    #[tokio::test]
    async fn ocr_find_and_exit_codes() {
        let lab = tempfile::tempdir().unwrap();
        let fake = FakeRunner::new(|c| {
            Ok(match c.args[1].as_str() {
                "ocr" => CmdOut::ok(b"Sign in\n".to_vec()),
                "find-image" if c.args.contains(&"0.8".to_owned()) => {
                    CmdOut::ok(b"x=10 y=20 w=30 h=40 score=0.97 center=25,40\n".to_vec())
                }
                "find-image" => CmdOut::fail(1, "no match"),
                "mouse-move" => CmdOut::fail(6, "unsupported"),
                _ => CmdOut::fail(4, "not found"),
            })
        });
        let c = ctx(fake.clone());
        let t = target(lab.path());
        let out = dispatch(
            &c,
            &t,
            &DisplayOp::Ocr {
                region: Some(Region {
                    x: 0,
                    y: 400,
                    w: 1024,
                    h: 200,
                }),
            },
        )
        .await
        .unwrap();
        assert_eq!(out.value["text"], "Sign in");
        assert_eq!(out.value["engine"], "vmlab");
        assert_eq!(
            fake.lines()[0],
            v(&[
                "vmlab", "vm", "ocr", "--region", "0", "400", "1024", "200", "win11"
            ])
        );
        let tpl = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png(2, 2));
        let found = dispatch(
            &c,
            &t,
            &DisplayOp::Find {
                image_base64: tpl.clone(),
                threshold: Some(0.8),
                region: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(found.value["found"], true);
        assert_eq!(found.value["center"], json!([25, 40]));
        assert_eq!(found.value["score"], 0.97);
        let none = dispatch(
            &c,
            &t,
            &DisplayOp::Find {
                image_base64: tpl,
                threshold: None,
                region: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(none.value["found"], false);
        let e = dispatch(&c, &t, &DisplayOp::Move { x: 1, y: 1 })
            .await
            .unwrap_err();
        assert_eq!(e.code(), ErrorCode::Unsupported);
        assert!(e.to_string().contains("no display"), "{e}");
        let e = dispatch(
            &c,
            &t,
            &DisplayOp::Drag {
                x1: 1,
                y1: 1,
                x2: 2,
                y2: 2,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::Failed);
        assert!(e.to_string().contains("no machine"), "{e}");
    }
}
