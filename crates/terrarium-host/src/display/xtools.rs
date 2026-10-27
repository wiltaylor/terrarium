//! The X11 and Wayland driver: the usual command-line tools, run in a
//! container (`docker exec`) or on this machine (a `desktop` target).
//!
//! | | X11 | Wayland (wlroots) |
//! |---|---|---|
//! | screenshot | `import -window root png:-`, else `xwd -root \| convert xwd:- png:-`, else `scrot` | `grim -` |
//! | pointer | `xdotool mousemove`, `click`, `mousedown`, `mouseup` | `ydotool mousemove --absolute`, `ydotool click` (needs `ydotoold`) |
//! | keys | `xdotool key ctrl+l` | `wtype -M ctrl -k l -m ctrl` |
//! | text | `xdotool type --file -` (text on stdin) | `wtype -` (text on stdin) |
//! | scroll | `xdotool click` of buttons 4 to 7 | `ydotool mousemove --wheel` |
//!
//! A missing tool is `unsupported`, and the message names the package to
//! install. The text of `type` goes on stdin, never on a command line.
//! GNOME and KDE on Wayland refuse synthetic input from these tools: use a
//! `vnc` or `rdp` target for them.

use std::sync::Arc;
use std::time::Duration;

use terrarium_core::display::{Button, DisplayProtocol, KeyChord, check_display_name};

use super::runner::{Cmd, CmdOut, Runner};
use super::{DResult, DisplayCtx, DisplayDriver, DisplayError, Features};
use crate::config::DisplayConfig;
use crate::verb::ident;

/// The desktop tools that `hello` reports for this machine.
pub const LOCAL_TOOLS: &[&str] = &[
    "xdotool", "import", "xwd", "convert", "scrot", "grim", "wtype", "ydotool",
];

/// The X11 screenshot script. It picks the first tool that exists, and
/// names the packages when there is none (exit 127).
pub const X11_SHOT: &str = r#"if command -v import >/dev/null 2>&1; then exec import -window root png:-
elif command -v xwd >/dev/null 2>&1 && command -v convert >/dev/null 2>&1; then xwd -root -silent | convert xwd:- png:-
elif command -v scrot >/dev/null 2>&1; then f=$(mktemp /tmp/terrarium-shot-XXXXXX) && scrot -o "$f.png" && cat "$f.png"; rc=$?; rm -f "$f" "$f.png"; exit $rc
else echo "no X11 screenshot tool: install imagemagick (import), or x11-apps (xwd) with imagemagick, or scrot" >&2; exit 127; fi"#;

/// The script that `display = "auto"` runs: `x11`, `wayland`, or nothing.
pub const DETECT: &str = "if command -v xdotool >/dev/null 2>&1; then echo x11; \
     elif command -v grim >/dev/null 2>&1; then echo wayland; fi";

/// Where the tools run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolHost {
    /// `docker exec` in a container.
    Docker { bin: String, container: String },
    /// This machine, with the environment of the `[display]` config.
    Local {
        xauthority: Option<String>,
        runtime_dir: Option<String>,
    },
}

impl ToolHost {
    pub fn docker(bin: &str, container: &str) -> DResult<Self> {
        let container = ident("container", container)
            .map_err(|e| DisplayError::BadRequest(e.message))?
            .to_owned();
        Ok(Self::Docker {
            bin: bin.to_owned(),
            container,
        })
    }

    pub fn local(cfg: &DisplayConfig) -> Self {
        let s = |p: &Option<std::path::PathBuf>| p.as_ref().map(|p| p.display().to_string());
        Self::Local {
            xauthority: s(&cfg.xauthority),
            runtime_dir: s(&cfg.runtime_dir),
        }
    }
}

/// The resolved display server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    X11,
    Wayland,
}

pub struct ToolDriver {
    runner: Arc<dyn Runner>,
    host: ToolHost,
    server: Server,
    display: String,
    timeout: Duration,
}

fn s(v: impl ToString) -> String {
    v.to_string()
}

impl ToolDriver {
    /// The driver for one screen. `auto` asks the container (or this
    /// machine) which tools it has.
    pub async fn open(
        ctx: &DisplayCtx,
        host: ToolHost,
        protocol: DisplayProtocol,
        display: Option<&str>,
    ) -> DResult<Self> {
        let mut d = Self {
            runner: ctx.runner.clone(),
            host,
            server: Server::X11,
            display: String::new(),
            timeout: ctx.timeout,
        };
        let protocol = match protocol {
            DisplayProtocol::Auto => d.detect().await?,
            p => p,
        };
        d.server = if protocol == DisplayProtocol::Wayland {
            Server::Wayland
        } else {
            Server::X11
        };
        let name = display.unwrap_or(protocol.default_display()).to_owned();
        check_display_name(&name).map_err(DisplayError::BadRequest)?;
        d.display = name;
        Ok(d)
    }

    pub fn server(&self) -> Server {
        self.server
    }

    async fn detect(&self) -> DResult<DisplayProtocol> {
        let out = self
            .exec_raw(Cmd::new("sh", vec![s("-c"), s(DETECT)]), false)
            .await?;
        match String::from_utf8_lossy(&out.stdout).trim() {
            "x11" => Ok(DisplayProtocol::X11),
            "wayland" => Ok(DisplayProtocol::Wayland),
            _ => Err(DisplayError::Unsupported(format!(
                "no display tools {}: install xdotool and imagemagick (X11), or grim, wtype, and \
                 ydotool (Wayland), or set display = \"x11\" or \"wayland\" on the target",
                self.where_()
            ))),
        }
    }

    fn where_(&self) -> String {
        match &self.host {
            ToolHost::Docker { container, .. } => format!("in container `{container}`"),
            ToolHost::Local { .. } => "on the host agent's machine".into(),
        }
    }

    /// The environment of the display server.
    fn env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        match self.server {
            Server::X11 => env.push((s("DISPLAY"), self.display.clone())),
            Server::Wayland => env.push((s("WAYLAND_DISPLAY"), self.display.clone())),
        }
        if let ToolHost::Local {
            xauthority,
            runtime_dir,
        } = &self.host
        {
            if let (Server::X11, Some(x)) = (self.server, xauthority) {
                env.push((s("XAUTHORITY"), x.clone()));
            }
            if let (Server::Wayland, Some(r)) = (self.server, runtime_dir) {
                env.push((s("XDG_RUNTIME_DIR"), r.clone()));
            }
        }
        env
    }

    /// The command as it runs: `docker exec [-i] -e K=V... C PROG ARGS`,
    /// or `PROG ARGS` with the env on this machine.
    /// The detect script runs with no display env (`with_env` false).
    pub fn build(&self, tool: Cmd, with_env: bool) -> Cmd {
        let env = if with_env { self.env() } else { Vec::new() };
        match &self.host {
            ToolHost::Docker { bin, container } => {
                let mut a = vec![s("exec")];
                if tool.stdin.is_some() {
                    a.push(s("-i"));
                }
                for (k, v) in &env {
                    a.push(s("-e"));
                    a.push(format!("{k}={v}"));
                }
                a.push(container.clone());
                a.push(tool.program);
                a.extend(tool.args);
                Cmd {
                    program: bin.clone(),
                    args: a,
                    env: Vec::new(),
                    cwd: None,
                    stdin: tool.stdin,
                    timeout: self.timeout,
                }
            }
            ToolHost::Local { .. } => Cmd {
                env,
                timeout: self.timeout,
                ..tool
            },
        }
    }

    async fn exec_raw(&self, tool: Cmd, with_env: bool) -> DResult<CmdOut> {
        let program = tool.program.clone();
        let cmd = self.build(tool, with_env);
        self.runner.run(cmd).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                missing(&program, &self.where_())
            } else {
                DisplayError::Failed(format!("{program}: {e}"))
            }
        })
    }

    /// Run a tool. Exit 127 (and docker's 126 or "executable file not
    /// found") is a missing tool.
    async fn run(
        &self,
        program: &str,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> DResult<CmdOut> {
        let mut tool = Cmd::new(program, args);
        tool.stdin = stdin;
        let out = self.exec_raw(tool, true).await?;
        if out.success() {
            return Ok(out);
        }
        let tail = out.stderr_tail();
        if matches!(out.code, Some(126 | 127)) || tail.contains("executable file not found") {
            if tail.starts_with("no X11 screenshot tool") {
                return Err(DisplayError::Unsupported(format!(
                    "{tail} {}",
                    self.where_()
                )));
            }
            return Err(missing(program, &self.where_()));
        }
        Err(DisplayError::Failed(format!(
            "{program} failed (exit {}) {}: {tail}",
            out.code.map_or("-".into(), |c| c.to_string()),
            self.where_()
        )))
    }

    fn wayland_pointer_note() -> &'static str {
        "ydotool needs ydotoold running and its socket (YDOTOOL_SOCKET) reachable"
    }
}

/// The error for a missing tool, with the package to install.
fn missing(program: &str, place: &str) -> DisplayError {
    let pkg = match program {
        "xdotool" => "xdotool",
        "import" | "convert" => "imagemagick",
        "xwd" => "x11-apps (xwd)",
        "grim" => "grim",
        "wtype" => "wtype",
        "ydotool" => "ydotool (and run ydotoold)",
        "sh" => "a POSIX sh",
        other => other,
    };
    DisplayError::Unsupported(format!("{program} is not available {place}: install {pkg}"))
}

impl DisplayDriver for ToolDriver {
    fn backend(&self) -> &'static str {
        match self.host {
            ToolHost::Docker { .. } => "docker",
            ToolHost::Local { .. } => "desktop",
        }
    }

    fn features(&self) -> Features {
        Features {
            scroll: true,
            ..Features::default()
        }
    }

    async fn screenshot(&self) -> DResult<Vec<u8>> {
        let out = match self.server {
            Server::X11 => self.run("sh", vec![s("-c"), s(X11_SHOT)], None).await?,
            Server::Wayland => self.run("grim", vec![s("-")], None).await?,
        };
        if !super::image::is_png(&out.stdout) {
            return Err(DisplayError::Failed(format!(
                "the screenshot tool gave no PNG {}: {}",
                self.where_(),
                out.stderr_tail()
            )));
        }
        Ok(out.stdout)
    }

    async fn move_to(&self, x: i32, y: i32) -> DResult<()> {
        match self.server {
            Server::X11 => {
                self.run("xdotool", vec![s("mousemove"), s(x), s(y)], None)
                    .await
            }
            Server::Wayland => {
                self.run(
                    "ydotool",
                    vec![
                        s("mousemove"),
                        s("--absolute"),
                        s("-x"),
                        s(x),
                        s("-y"),
                        s(y),
                    ],
                    None,
                )
                .await
            }
        }
        .map(|_| ())
    }

    async fn click(&self, at: Option<(i32, i32)>, button: Button, double: bool) -> DResult<()> {
        match self.server {
            Server::X11 => {
                let mut a = Vec::new();
                if let Some((x, y)) = at {
                    a.extend([s("mousemove"), s(x), s(y)]);
                }
                a.push(s("click"));
                if double {
                    a.extend([s("--repeat"), s(2)]);
                }
                a.push(s(button.x_number()));
                self.run("xdotool", a, None).await.map(|_| ())
            }
            Server::Wayland => {
                if let Some((x, y)) = at {
                    self.move_to(x, y).await?;
                }
                // ydotool 1.0: 0xC0 + n is a press and a release of button n
                // (0 left, 1 right, 2 middle).
                let code = match button {
                    Button::Left => "0xC0",
                    Button::Right => "0xC1",
                    Button::Middle => "0xC2",
                };
                let mut a = vec![s("click")];
                if double {
                    a.extend([s("--repeat"), s(2)]);
                }
                a.push(s(code));
                self.run("ydotool", a, None).await.map(|_| ())
            }
        }
    }

    async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> DResult<()> {
        match self.server {
            Server::X11 => self
                .run(
                    "xdotool",
                    vec![
                        s("mousemove"),
                        s(from.0),
                        s(from.1),
                        s("mousedown"),
                        s(1),
                        s("sleep"),
                        s("0.1"),
                        s("mousemove"),
                        s(to.0),
                        s(to.1),
                        s("sleep"),
                        s("0.1"),
                        s("mouseup"),
                        s(1),
                    ],
                    None,
                )
                .await
                .map(|_| ()),
            Server::Wayland => {
                self.move_to(from.0, from.1).await?;
                // 0x40: press the left button; 0x80: release it.
                self.run("ydotool", vec![s("click"), s("0x40")], None)
                    .await?;
                self.move_to(to.0, to.1).await?;
                self.run("ydotool", vec![s("click"), s("0x80")], None)
                    .await
                    .map(|_| ())
            }
        }
    }

    async fn scroll(&self, at: Option<(i32, i32)>, dx: i32, dy: i32) -> DResult<()> {
        match self.server {
            Server::X11 => {
                let mut a = Vec::new();
                if let Some((x, y)) = at {
                    a.extend([s("mousemove"), s(x), s(y)]);
                }
                // Buttons 4 and 5 are up and down, 6 and 7 left and right.
                for (n, up, down) in [(dy, 4, 5), (dx, 6, 7)] {
                    if n != 0 {
                        a.extend([
                            s("click"),
                            s("--repeat"),
                            s(n.abs()),
                            s(if n < 0 { up } else { down }),
                        ]);
                    }
                }
                self.run("xdotool", a, None).await.map(|_| ())
            }
            Server::Wayland => {
                if let Some((x, y)) = at {
                    self.move_to(x, y).await?;
                }
                // ydotool's wheel: a negative y scrolls down.
                self.run(
                    "ydotool",
                    vec![
                        s("mousemove"),
                        s("--wheel"),
                        s("-x"),
                        s(dx),
                        s("-y"),
                        s(-dy),
                    ],
                    None,
                )
                .await
                .map(|_| ())
            }
        }
    }

    async fn keys(&self, chord: &KeyChord) -> DResult<()> {
        match self.server {
            Server::X11 => {
                self.run("xdotool", vec![s("key"), chord.xdotool()], None)
                    .await
            }
            Server::Wayland => self.run("wtype", chord.wtype_args(), None).await,
        }
        .map(|_| ())
    }

    async fn type_text(&self, text: &str) -> DResult<()> {
        let stdin = Some(text.as_bytes().to_vec());
        match self.server {
            Server::X11 => {
                self.run(
                    "xdotool",
                    vec![s("type"), s("--delay"), s(12), s("--file"), s("-")],
                    stdin,
                )
                .await
            }
            Server::Wayland => self.run("wtype", vec![s("-")], stdin).await,
        }
        .map(|_| ())
        .map_err(|e| match (self.server, e) {
            (Server::Wayland, DisplayError::Failed(m)) => {
                DisplayError::Failed(format!("{m} ({})", Self::wayland_pointer_note()))
            }
            (_, e) => e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::dispatch;
    use super::super::runner::FakeRunner;
    use super::super::testing::{ctx, png};
    use super::*;
    use crate::protocol::{DisplayTarget, ErrorCode};
    use terrarium_core::display::DisplayOp;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    fn docker(protocol: DisplayProtocol) -> DisplayTarget {
        DisplayTarget::Docker {
            container: "terrarium-desk".into(),
            protocol,
            display: None,
        }
    }

    async fn lines(protocol: DisplayProtocol, op: DisplayOp) -> Vec<Vec<String>> {
        let fake = FakeRunner::ok();
        dispatch(&ctx(fake.clone()), &docker(protocol), &op)
            .await
            .unwrap();
        fake.lines()
    }

    #[tokio::test]
    async fn x11_command_lines() {
        let x = DisplayProtocol::X11;
        let pre = ["docker", "exec", "-e", "DISPLAY=:0", "terrarium-desk"];
        let with = |rest: &[&str]| {
            let mut l = v(&pre);
            l.extend(v(rest));
            l
        };
        assert_eq!(
            lines(x, DisplayOp::Move { x: 3, y: 4 }).await,
            [with(&["xdotool", "mousemove", "3", "4"])]
        );
        assert_eq!(
            lines(
                x,
                DisplayOp::Click {
                    x: Some(1),
                    y: Some(2),
                    button: Button::Middle,
                    double: true
                }
            )
            .await,
            [with(&[
                "xdotool",
                "mousemove",
                "1",
                "2",
                "click",
                "--repeat",
                "2",
                "2"
            ])]
        );
        assert_eq!(
            lines(
                x,
                DisplayOp::Scroll {
                    x: None,
                    y: None,
                    dx: -1,
                    dy: 3
                }
            )
            .await,
            [with(&[
                "xdotool", "click", "--repeat", "3", "5", "click", "--repeat", "1", "6"
            ])]
        );
        assert_eq!(
            lines(
                x,
                DisplayOp::Keys {
                    chord: "ctrl-l".into()
                }
            )
            .await,
            [with(&["xdotool", "key", "ctrl+l"])]
        );
        assert_eq!(
            lines(
                x,
                DisplayOp::Drag {
                    x1: 1,
                    y1: 2,
                    x2: 3,
                    y2: 4
                }
            )
            .await[0][5..9],
            v(&["xdotool", "mousemove", "1", "2"])
        );
    }

    #[tokio::test]
    async fn typed_text_goes_on_stdin_only() {
        let fake = FakeRunner::ok();
        dispatch(
            &ctx(fake.clone()),
            &docker(DisplayProtocol::X11),
            &DisplayOp::Type {
                text: "hunter2".into(),
            },
        )
        .await
        .unwrap();
        let cmd = &fake.cmds()[0];
        assert_eq!(
            cmd.line(),
            v(&[
                "docker",
                "exec",
                "-i",
                "-e",
                "DISPLAY=:0",
                "terrarium-desk",
                "xdotool",
                "type",
                "--delay",
                "12",
                "--file",
                "-"
            ])
        );
        assert_eq!(cmd.stdin.as_deref(), Some(b"hunter2".as_slice()));
        assert!(!cmd.line().join(" ").contains("hunter2"));
    }

    #[tokio::test]
    async fn wayland_command_lines() {
        let w = DisplayProtocol::Wayland;
        let pre = [
            "docker",
            "exec",
            "-e",
            "WAYLAND_DISPLAY=wayland-0",
            "terrarium-desk",
        ];
        let with = |rest: &[&str]| {
            let mut l = v(&pre);
            l.extend(v(rest));
            l
        };
        assert_eq!(
            lines(
                w,
                DisplayOp::Keys {
                    chord: "ctrl+shift+t".into()
                }
            )
            .await,
            [with(&[
                "wtype", "-M", "ctrl", "-M", "shift", "-k", "t", "-m", "shift", "-m", "ctrl"
            ])]
        );
        assert_eq!(
            lines(
                w,
                DisplayOp::Click {
                    x: Some(5),
                    y: Some(6),
                    button: Button::Right,
                    double: false
                }
            )
            .await,
            [
                with(&["ydotool", "mousemove", "--absolute", "-x", "5", "-y", "6"]),
                with(&["ydotool", "click", "0xC1"]),
            ]
        );
    }

    #[tokio::test]
    async fn screenshots_use_the_script_or_grim() {
        let shot = png(4, 4);
        let s2 = shot.clone();
        let fake = FakeRunner::new(move |_| Ok(CmdOut::ok(s2.clone())));
        let c = ctx(fake.clone());
        let op = DisplayOp::Screenshot {
            region: None,
            scale: None,
        };
        dispatch(&c, &docker(DisplayProtocol::X11), &op)
            .await
            .unwrap();
        dispatch(&c, &docker(DisplayProtocol::Wayland), &op)
            .await
            .unwrap();
        let l = fake.lines();
        assert_eq!(l[0][5..], [s("sh"), s("-c"), s(X11_SHOT)]);
        assert_eq!(l[1][5..], [s("grim"), s("-")]);
    }

    #[tokio::test]
    async fn missing_tools_name_the_package() {
        let fake = FakeRunner::new(|c| {
            Ok(if c.args.contains(&s("xdotool")) {
                CmdOut::fail(
                    127,
                    "OCI runtime exec failed: exec: \"xdotool\": executable file not found in $PATH",
                )
            } else {
                CmdOut::fail(
                    127,
                    "no X11 screenshot tool: install imagemagick (import), or x11-apps (xwd) \
                     with imagemagick, or scrot",
                )
            })
        });
        let c = ctx(fake);
        let t = docker(DisplayProtocol::X11);
        let e = dispatch(&c, &t, &DisplayOp::Move { x: 1, y: 1 })
            .await
            .unwrap_err();
        assert_eq!(e.code(), ErrorCode::Unsupported);
        assert_eq!(
            e.to_string(),
            "xdotool is not available in container `terrarium-desk`: install xdotool"
        );
        let e = dispatch(&c, &t, &DisplayOp::Info).await.unwrap_err();
        assert_eq!(e.code(), ErrorCode::Unsupported);
        assert!(e.to_string().contains("install imagemagick"), "{e}");
    }

    #[tokio::test]
    async fn auto_detects_the_server() {
        let fake = FakeRunner::new(|c| {
            Ok(if c.args.iter().any(|a| a == DETECT) {
                CmdOut::ok(b"wayland\n".to_vec())
            } else {
                CmdOut::ok(Vec::new())
            })
        });
        let c = ctx(fake.clone());
        dispatch(
            &c,
            &docker(DisplayProtocol::Auto),
            &DisplayOp::Keys { chord: "a".into() },
        )
        .await
        .unwrap();
        let l = fake.lines();
        assert_eq!(
            l[0],
            v(&["docker", "exec", "terrarium-desk", "sh", "-c", DETECT])
        );
        assert_eq!(l[1][4..], v(&["terrarium-desk", "wtype", "-k", "a"]));

        let none = FakeRunner::ok();
        let e = dispatch(&ctx(none), &docker(DisplayProtocol::Auto), &DisplayOp::Info)
            .await
            .unwrap_err();
        assert_eq!(e.code(), ErrorCode::Unsupported);
        assert!(e.to_string().contains("install xdotool"), "{e}");
    }

    #[tokio::test]
    async fn the_desktop_runs_locally_with_its_env() {
        let fake = FakeRunner::ok();
        let mut c = ctx(fake.clone());
        c.config.enabled = true;
        c.config.xauthority = Some("/home/u/.Xauthority".into());
        dispatch(
            &c,
            &DisplayTarget::Desktop {
                protocol: DisplayProtocol::X11,
                display: Some(":1".into()),
            },
            &DisplayOp::Move { x: 1, y: 2 },
        )
        .await
        .unwrap();
        let cmd = &fake.cmds()[0];
        assert_eq!(cmd.line(), v(&["xdotool", "mousemove", "1", "2"]));
        assert_eq!(
            cmd.env,
            [
                (s("DISPLAY"), s(":1")),
                (s("XAUTHORITY"), s("/home/u/.Xauthority"))
            ]
        );
    }

    /// The real tools in a container of `examples/display/x11.Dockerfile`.
    /// Run with `TERRARIUM_X11_CONTAINER=<name> cargo test -p terrarium-host
    /// live_x11 -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a running examples/display/x11.Dockerfile container"]
    async fn live_x11_container() {
        let Ok(container) = std::env::var("TERRARIUM_X11_CONTAINER") else {
            eprintln!("TERRARIUM_X11_CONTAINER is not set; skipped");
            return;
        };
        let mut c = ctx(FakeRunner::ok());
        c.runner = Arc::new(super::super::runner::SystemRunner);
        c.timeout = Duration::from_secs(20);
        let t = DisplayTarget::Docker {
            container,
            protocol: DisplayProtocol::Auto,
            display: None,
        };
        let info = dispatch(&c, &t, &DisplayOp::Info).await.unwrap();
        eprintln!("info: {}", info.value);
        assert_eq!(info.value["width"], 1280);
        let shot = dispatch(
            &c,
            &t,
            &DisplayOp::Screenshot {
                region: Some(terrarium_core::display::Region {
                    x: 0,
                    y: 0,
                    w: 800,
                    h: 600,
                }),
                scale: Some(0.5),
            },
        )
        .await
        .unwrap();
        assert_eq!(shot.value["width"], 400);
        for op in [
            DisplayOp::Move { x: 200, y: 200 },
            DisplayOp::Click {
                x: Some(300),
                y: Some(300),
                button: Button::Left,
                double: true,
            },
            DisplayOp::Type {
                text: "echo 'hi there' && date\n".into(),
            },
            DisplayOp::Keys {
                chord: "ctrl+l".into(),
            },
            DisplayOp::Drag {
                x1: 100,
                y1: 100,
                x2: 150,
                y2: 120,
            },
            DisplayOp::Scroll {
                x: Some(400),
                y: Some(300),
                dx: 0,
                dy: 2,
            },
        ] {
            let out = dispatch(&c, &t, &op).await;
            assert!(out.is_ok(), "{op:?}: {out:?}");
        }
    }

    #[tokio::test]
    async fn bad_names_are_refused() {
        let fake = FakeRunner::ok();
        let c = ctx(fake.clone());
        let bad = DisplayTarget::Docker {
            container: "-it".into(),
            protocol: DisplayProtocol::X11,
            display: None,
        };
        assert!(dispatch(&c, &bad, &DisplayOp::Info).await.is_err());
        let bad = DisplayTarget::Docker {
            container: "c".into(),
            protocol: DisplayProtocol::X11,
            display: Some(":0;rm".into()),
        };
        assert!(dispatch(&c, &bad, &DisplayOp::Info).await.is_err());
        assert!(fake.lines().is_empty());
    }
}
