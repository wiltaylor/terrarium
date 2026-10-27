//! Target displays: screenshot, mouse, and keyboard on a target's screen
//! (PLAN §19b, docs/host-protocol.md "`display`").
//!
//! The daemon sends one `display` frame for each operation. The host agent
//! picks a driver from the frame's `target` and runs the operation with
//! [`run`]:
//!
//! | Backend | Driver | Tools |
//! |---|---|---|
//! | `vmlab` | [`vmlab::VmlabDriver`] | `vmlab vm screenshot\|mouse-move\|click\|drag\|sendkeys\|ocr\|find-image`, `vmlab record` |
//! | `docker` | [`xtools::ToolDriver`] over `docker exec` | X11: `xdotool`, and `import`, `xwd` with `convert`, or `scrot`. Wayland: `grim`, `wtype`, `ydotool` |
//! | `desktop` | [`xtools::ToolDriver`] on this machine | the same tools; only with `[display] enabled = true` |
//! | `vnc` | [`remote::VncDriver`] | none: an RFB client in the host agent ([`vnc`]) |
//! | `rdp` | [`remote::RdpDriver`] | none: an RDP client on IronRDP in the host agent ([`rdp`]) |
//!
//! # Adding a driver
//!
//! A driver implements [`DisplayDriver`]: a full-screen PNG, the pointer,
//! the keys, and text. The engine ([`run_op`]) does the rest the same way
//! for every driver: it checks the values, crops and scales screenshots
//! ([`image`]), runs `tesseract` when the driver has no OCR of its own,
//! and makes the `result` value. A driver overrides [`DisplayDriver::ocr`],
//! [`DisplayDriver::find`], and [`DisplayDriver::record`] only when its
//! backend has them. The VNC and RDP drivers keep one connection for each
//! target in [`remote::Remotes`] ([`DisplayCtx::remotes`]).

pub mod image;
pub mod rdp;
pub mod remote;
pub mod runner;
pub mod vmlab;
pub mod vnc;
pub mod xtools;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use terrarium_core::display::{
    Button, DisplayOp, DisplayProtocol, KeyChord, MAX_FIND_TEMPLATE_BYTES, MAX_SCREENSHOT_BYTES,
    RecordAction, Region,
};
use terrarium_core::protocol::DisplayCapability;

use crate::config::{DisplayConfig, PathPolicy};
use crate::protocol::{DisplayTarget, ErrorCode};
use image::Image;
use runner::{Cmd, Runner};

/// The default limit of one display operation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a display operation failed. Each becomes an `error` frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DisplayError {
    /// A value is not valid. `bad_request`.
    #[error("{0}")]
    BadRequest(String),
    /// The backend cannot do this, or a tool is missing. `unsupported`.
    #[error("{0}")]
    Unsupported(String),
    /// The host agent refuses: its desktop is off, or the display is not
    /// in `allowed_displays`. `refused`.
    #[error("{0}")]
    Refused(String),
    /// A tool ran and failed. `failed`.
    #[error("{0}")]
    Failed(String),
    /// The driver has no native form of the operation; the engine uses
    /// its fallback (`tesseract` for OCR). Never leaves the host agent.
    #[error("the driver has no native form of this operation")]
    NotNative,
}

impl DisplayError {
    /// The code of the `error` frame.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::BadRequest(_) => ErrorCode::BadRequest,
            Self::Unsupported(_) | Self::NotNative => ErrorCode::Unsupported,
            Self::Refused(_) => ErrorCode::Refused,
            Self::Failed(_) => ErrorCode::Failed,
        }
    }
}

pub type DResult<T> = Result<T, DisplayError>;

/// What a driver does by itself, for `info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub scroll: bool,
    /// Its own OCR (else the engine uses `tesseract`, when present).
    pub ocr: bool,
    pub find: bool,
    pub record: bool,
}

/// A display driver: one screen of one backend.
///
/// Every method is one action on the screen. Coordinates are absolute
/// pixels from the top-left corner of the full screen; the engine checked
/// them already ([`DisplayOp::check`]).
pub trait DisplayDriver: Send + Sync {
    /// The backend name: `vmlab`, `docker`, `desktop`, `vnc`, or `rdp`.
    fn backend(&self) -> &'static str;

    /// What the driver does by itself.
    fn features(&self) -> Features {
        Features::default()
    }

    /// A PNG of the full screen.
    fn screenshot(&self) -> impl Future<Output = DResult<Vec<u8>>> + Send;

    /// Move the pointer to `x, y`.
    fn move_to(&self, x: i32, y: i32) -> impl Future<Output = DResult<()>> + Send;

    /// Click `button` at `at` (or where the pointer is); two clicks when
    /// `double`.
    fn click(
        &self,
        at: Option<(i32, i32)>,
        button: Button,
        double: bool,
    ) -> impl Future<Output = DResult<()>> + Send;

    /// Press the left button at `from`, move to `to`, release.
    fn drag(&self, from: (i32, i32), to: (i32, i32)) -> impl Future<Output = DResult<()>> + Send;

    /// Scroll `dx`, `dy` wheel steps (a positive `dy` scrolls down) at
    /// `at` (or where the pointer is).
    fn scroll(
        &self,
        at: Option<(i32, i32)>,
        dx: i32,
        dy: i32,
    ) -> impl Future<Output = DResult<()>> + Send;

    /// Press and release one chord.
    fn keys(&self, chord: &KeyChord) -> impl Future<Output = DResult<()>> + Send;

    /// Type text. A backend with no text input sends a chord for each
    /// character ([`terrarium_core::display::us_char_chord`]).
    fn type_text(&self, text: &str) -> impl Future<Output = DResult<()>> + Send;

    /// The text on the screen, or in `region`. The default is
    /// [`DisplayError::NotNative`]: the engine runs `tesseract` on a
    /// screenshot.
    fn ocr(&self, _region: Option<Region>) -> impl Future<Output = DResult<String>> + Send {
        async { Err(DisplayError::NotNative) }
    }

    /// Find a template PNG on the screen. `{"found", "x", "y", "w", "h",
    /// "score", "center"}`.
    fn find(
        &self,
        _template: &[u8],
        _threshold: Option<f32>,
        _region: Option<Region>,
    ) -> impl Future<Output = DResult<Value>> + Send {
        let b = self.backend();
        async move {
            Err(DisplayError::Unsupported(format!(
                "the {b} display has no image search; take a screenshot and look at it"
            )))
        }
    }

    /// Start or stop a recording of the screen.
    fn record(&self, _action: RecordAction) -> impl Future<Output = DResult<Value>> + Send {
        let b = self.backend();
        async move {
            Err(DisplayError::Unsupported(format!(
                "the {b} display has no recording; only vmlab targets record"
            )))
        }
    }
}

/// The answer of one operation: the `result` value, and the PNG of a
/// screenshot (sent as `stdout` data frames).
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayOutput {
    pub value: Value,
    pub png: Option<Vec<u8>>,
}

impl DisplayOutput {
    fn value(value: Value) -> Self {
        Self { value, png: None }
    }
}

/// Everything [`run`] needs from the host agent.
#[derive(Clone)]
pub struct DisplayCtx {
    pub runner: Arc<dyn Runner>,
    pub docker_bin: String,
    pub docker_ok: bool,
    pub vmlab_bin: String,
    pub vmlab_ok: bool,
    pub policy: PathPolicy,
    /// `[display]` of the config: the opt-in for `desktop` targets.
    pub config: DisplayConfig,
    /// `tesseract` on this machine, for OCR.
    pub tesseract: bool,
    pub timeout: Duration,
    /// The kept VNC and RDP connections, and the RDP certificate pins.
    pub remotes: Arc<remote::Remotes>,
}

fn no_ocr() -> DisplayError {
    DisplayError::Unsupported(
        "no OCR: this display has no OCR of its own, and tesseract is not on the host \
         agent's machine (install tesseract-ocr); take a screenshot and read it instead"
            .into(),
    )
}

/// The `tesseract` OCR of a PNG: `tesseract stdin stdout`.
async fn tesseract(ctx: &DisplayCtx, png: Vec<u8>) -> DResult<String> {
    if !ctx.tesseract {
        return Err(no_ocr());
    }
    let mut cmd = Cmd::new(
        ctx.config.tesseract_bin.clone(),
        vec!["stdin".into(), "stdout".into()],
    );
    cmd.stdin = Some(png);
    cmd.timeout = ctx.timeout;
    let out = ctx
        .runner
        .run(cmd)
        .await
        .map_err(|e| DisplayError::Failed(format!("tesseract: {e}")))?;
    if !out.success() {
        return Err(DisplayError::Failed(format!(
            "tesseract failed: {}",
            out.stderr_tail()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

fn sha256(bytes: &[u8]) -> String {
    crate::protocol::hex(&Sha256::digest(bytes))
}

fn xy(x: Option<i32>, y: Option<i32>) -> Option<(i32, i32)> {
    x.zip(y)
}

/// Run one operation on a driver: the engine part that is the same for
/// every backend.
pub async fn run_op<D: DisplayDriver>(
    d: &D,
    op: &DisplayOp,
    ctx: &DisplayCtx,
) -> DResult<DisplayOutput> {
    op.check().map_err(DisplayError::BadRequest)?;
    let ok = |extra: Value| {
        let mut v = json!({"op": op.name(), "ok": true});
        if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
            m.extend(e);
        }
        DisplayOutput::value(v)
    };
    match op {
        DisplayOp::Info => {
            let shot = d.screenshot().await?;
            let (w, h) = image::dims(&shot)?;
            let f = d.features();
            let mut ops = vec!["info", "screenshot", "move", "click", "drag"];
            if f.scroll {
                ops.push("scroll");
            }
            ops.extend(["keys", "type"]);
            let ocr = if f.ocr {
                Some(d.backend())
            } else if ctx.tesseract {
                Some("tesseract")
            } else {
                None
            };
            if ocr.is_some() {
                ops.push("ocr");
            }
            if f.find {
                ops.push("find");
            }
            if f.record {
                ops.push("record");
            }
            Ok(DisplayOutput::value(json!({
                "op": "info", "backend": d.backend(), "width": w, "height": h,
                "ops": ops, "ocr": ocr,
            })))
        }
        DisplayOp::Screenshot { region, scale } => {
            let shot = d.screenshot().await?;
            let (sw, sh) = image::dims(&shot)?;
            let png = if region.is_none() && scale.is_none_or(|s| s >= 1.0) {
                shot
            } else {
                let mut img = Image::decode(&shot)?;
                if let Some(r) = region {
                    img = img.crop(*r)?;
                }
                if let Some(s) = scale {
                    img = img.scale(*s)?;
                }
                img.encode()?
            };
            if png.len() > MAX_SCREENSHOT_BYTES {
                return Err(DisplayError::Failed(format!(
                    "the screenshot is {} bytes, over the {MAX_SCREENSHOT_BYTES} byte limit; \
                     ask for a region or a smaller scale",
                    png.len()
                )));
            }
            let (w, h) = image::dims(&png)?;
            Ok(DisplayOutput {
                value: json!({
                    "op": "screenshot", "format": "png", "width": w, "height": h,
                    "screen": {"width": sw, "height": sh},
                    "region": region, "scale": scale,
                    "bytes": png.len(), "sha256": sha256(&png),
                }),
                png: Some(png),
            })
        }
        DisplayOp::Move { x, y } => {
            d.move_to(*x, *y).await?;
            Ok(ok(json!({"x": x, "y": y})))
        }
        DisplayOp::Click {
            x,
            y,
            button,
            double,
        } => {
            d.click(xy(*x, *y), *button, *double).await?;
            Ok(ok(
                json!({"x": x, "y": y, "button": button, "double": double}),
            ))
        }
        DisplayOp::Drag { x1, y1, x2, y2 } => {
            d.drag((*x1, *y1), (*x2, *y2)).await?;
            Ok(ok(json!({"x1": x1, "y1": y1, "x2": x2, "y2": y2})))
        }
        DisplayOp::Scroll { x, y, dx, dy } => {
            if !d.features().scroll {
                return Err(DisplayError::Unsupported(format!(
                    "the {} display has no scroll input; use keys pagedown or pageup, or a drag",
                    d.backend()
                )));
            }
            d.scroll(xy(*x, *y), *dx, *dy).await?;
            Ok(ok(json!({"x": x, "y": y, "dx": dx, "dy": dy})))
        }
        DisplayOp::Keys { chord } => {
            let c = KeyChord::parse(chord).map_err(DisplayError::BadRequest)?;
            d.keys(&c).await?;
            Ok(ok(json!({"chord": c.canonical()})))
        }
        DisplayOp::Type { text } => {
            d.type_text(text).await?;
            Ok(ok(json!({"chars": text.chars().count()})))
        }
        DisplayOp::Ocr { region } => match d.ocr(*region).await {
            Ok(text) => Ok(DisplayOutput::value(
                json!({"op": "ocr", "engine": d.backend(), "text": text, "region": region}),
            )),
            Err(DisplayError::NotNative) => {
                if !ctx.tesseract {
                    return Err(no_ocr());
                }
                let shot = d.screenshot().await?;
                let png = match region {
                    Some(r) => Image::decode(&shot)?.crop(*r)?.encode()?,
                    None => shot,
                };
                let text = tesseract(ctx, png).await?;
                Ok(DisplayOutput::value(
                    json!({"op": "ocr", "engine": "tesseract", "text": text, "region": region}),
                ))
            }
            Err(e) => Err(e),
        },
        DisplayOp::Find {
            image_base64,
            threshold,
            region,
        } => {
            let png = base64::engine::general_purpose::STANDARD
                .decode(image_base64.trim())
                .map_err(|e| DisplayError::BadRequest(format!("image_base64: {e}")))?;
            if png.len() > MAX_FIND_TEMPLATE_BYTES || !image::is_png(&png) {
                return Err(DisplayError::BadRequest(format!(
                    "the template must be a PNG of at most {} KiB",
                    MAX_FIND_TEMPLATE_BYTES / 1024
                )));
            }
            let mut v = d.find(&png, *threshold, *region).await?;
            if let Value::Object(m) = &mut v {
                m.insert("op".into(), json!("find"));
            }
            Ok(DisplayOutput::value(v))
        }
        DisplayOp::Record { action } => {
            let mut v = d.record(*action).await?;
            if let Value::Object(m) = &mut v {
                m.insert("op".into(), json!("record"));
                m.insert("action".into(), json!(action));
            }
            Ok(DisplayOutput::value(v))
        }
    }
}

/// The display name of a `desktop` frame, checked against the opt-in.
fn desktop_display(
    cfg: &DisplayConfig,
    protocol: DisplayProtocol,
    display: Option<&str>,
) -> DResult<String> {
    if !cfg.enabled {
        return Err(DisplayError::Refused(
            "the desktop display of this host is off: the host agent's own config needs \
             [display] enabled = true (the daemon cannot turn it on)"
                .into(),
        ));
    }
    let name = display
        .map(str::to_owned)
        .or_else(|| cfg.default_display.clone())
        .unwrap_or_else(|| protocol.default_display().to_owned());
    terrarium_core::display::check_display_name(&name).map_err(DisplayError::BadRequest)?;
    if !cfg.allowed_displays.is_empty() && !cfg.allowed_displays.contains(&name) {
        return Err(DisplayError::Refused(format!(
            "display {name} is not in allowed_displays of the host agent's [display] config"
        )));
    }
    Ok(name)
}

/// Build the driver for a frame and run the operation.
pub async fn dispatch(
    ctx: &DisplayCtx,
    target: &DisplayTarget,
    op: &DisplayOp,
) -> DResult<DisplayOutput> {
    match target {
        DisplayTarget::Vmlab { lab, machine } => {
            if !ctx.vmlab_ok {
                return Err(DisplayError::Unsupported(
                    "vmlab is not available on this host".into(),
                ));
            }
            let d = vmlab::VmlabDriver::new(ctx, lab, machine)?;
            run_op(&d, op, ctx).await
        }
        DisplayTarget::Docker {
            container,
            protocol,
            display,
        } => {
            if !ctx.docker_ok {
                return Err(DisplayError::Unsupported(
                    "docker is not available on this host".into(),
                ));
            }
            let host = xtools::ToolHost::docker(&ctx.docker_bin, container)?;
            let d = xtools::ToolDriver::open(ctx, host, *protocol, display.as_deref()).await?;
            run_op(&d, op, ctx).await
        }
        DisplayTarget::Desktop { protocol, display } => {
            let name = desktop_display(&ctx.config, *protocol, display.as_deref())?;
            let host = xtools::ToolHost::local(&ctx.config);
            let d = xtools::ToolDriver::open(ctx, host, *protocol, Some(&name)).await?;
            run_op(&d, op, ctx).await
        }
        DisplayTarget::Vnc {
            address,
            username,
            password,
        } => {
            let d = remote::VncDriver::open(ctx, address, username.as_deref(), password.clone())
                .await?;
            run_op(&d, op, ctx).await
        }
        DisplayTarget::Rdp {
            address,
            username,
            domain,
            password,
        } => {
            let d = remote::RdpDriver::open(
                ctx,
                address,
                username.clone(),
                domain.clone(),
                password.clone(),
            )
            .await?;
            run_op(&d, op, ctx).await
        }
    }
}

/// The display part of `hello`: the opt-in, `tesseract`, the desktop
/// tools on `PATH`, and the drivers.
pub fn detect(cfg: &DisplayConfig, docker: bool, vmlab: bool) -> DisplayCapability {
    let tools: Vec<String> = xtools::LOCAL_TOOLS
        .iter()
        .filter(|t| on_path(t))
        .map(|t| (*t).to_owned())
        .collect();
    let mut drivers = Vec::new();
    if vmlab {
        drivers.push("vmlab".to_owned());
    }
    if docker {
        drivers.push("docker".to_owned());
    }
    if cfg.enabled {
        drivers.push("desktop".to_owned());
    }
    // The VNC and RDP clients are in the host agent: no tool to look for.
    drivers.push("vnc".to_owned());
    drivers.push("rdp".to_owned());
    DisplayCapability {
        enabled: cfg.enabled,
        allowed_displays: cfg.allowed_displays.clone(),
        tesseract: on_path(&cfg.tesseract_bin),
        tools,
        drivers,
    }
}

/// True when `bin` is a path to a file, or a file in a `PATH` directory.
pub fn on_path(bin: &str) -> bool {
    if bin.contains('/') {
        return std::path::Path::new(bin).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use runner::FakeRunner;

    /// A context with a fake runner, docker and vmlab present.
    pub fn ctx(runner: FakeRunner) -> DisplayCtx {
        DisplayCtx {
            runner: Arc::new(runner),
            docker_bin: "docker".into(),
            docker_ok: true,
            vmlab_bin: "vmlab".into(),
            vmlab_ok: true,
            policy: PathPolicy::default(),
            config: DisplayConfig::default(),
            tesseract: false,
            timeout: Duration::from_secs(5),
            remotes: remote::Remotes::new(
                std::env::temp_dir().join(format!("terrarium-test-pins-{}", std::process::id())),
            ),
        }
    }

    /// A small PNG.
    pub fn png(w: u32, h: u32) -> Vec<u8> {
        image::two_tone(w, h).encode().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{ctx, png};
    use super::*;
    use runner::{CmdOut, FakeRunner};

    #[tokio::test]
    async fn the_desktop_is_refused_until_the_config_opts_in() {
        let fake = FakeRunner::ok();
        let mut c = ctx(fake.clone());
        let target = DisplayTarget::Desktop {
            protocol: DisplayProtocol::X11,
            display: None,
        };
        let e = dispatch(&c, &target, &DisplayOp::Info).await.unwrap_err();
        assert_eq!(e.code(), ErrorCode::Refused);
        assert!(e.to_string().contains("[display] enabled = true"), "{e}");
        assert!(fake.lines().is_empty(), "nothing ran");

        c.config.enabled = true;
        c.config.allowed_displays = vec![":1".into()];
        let e = dispatch(&c, &target, &DisplayOp::Info).await.unwrap_err();
        assert_eq!(e.code(), ErrorCode::Refused);
        assert!(e.to_string().contains("allowed_displays"), "{e}");
    }

    #[test]
    fn hello_reports_the_opt_in() {
        let off = detect(&DisplayConfig::default(), true, true);
        assert!(!off.enabled);
        assert_eq!(off.drivers, ["vmlab", "docker", "vnc", "rdp"]);
        let on = detect(
            &DisplayConfig {
                enabled: true,
                ..DisplayConfig::default()
            },
            false,
            false,
        );
        assert!(on.enabled);
        assert_eq!(on.drivers, ["desktop", "vnc", "rdp"]);
    }

    #[tokio::test]
    async fn ocr_falls_back_to_tesseract_or_says_no_ocr() {
        let shot = png(8, 8);
        let s2 = shot.clone();
        let fake = FakeRunner::new(move |c| {
            if c.program == "tesseract" {
                assert_eq!(c.args, ["stdin", "stdout"]);
                assert!(c.stdin.is_some());
                Ok(CmdOut::ok(b"Hello world\n".to_vec()))
            } else {
                Ok(CmdOut::ok(s2.clone()))
            }
        });
        let mut c = ctx(fake.clone());
        let target = DisplayTarget::Docker {
            container: "c".into(),
            protocol: DisplayProtocol::X11,
            display: None,
        };
        let op = DisplayOp::Ocr { region: None };
        let e = dispatch(&c, &target, &op).await.unwrap_err();
        assert_eq!(e.code(), ErrorCode::Unsupported);
        assert!(e.to_string().starts_with("no OCR"), "{e}");

        c.tesseract = true;
        let out = dispatch(&c, &target, &op).await.unwrap();
        assert_eq!(out.value["text"], "Hello world");
        assert_eq!(out.value["engine"], "tesseract");
    }

    #[tokio::test]
    async fn screenshots_crop_and_scale_and_report_the_screen() {
        let shot = png(40, 20);
        let fake = FakeRunner::new(move |_| Ok(CmdOut::ok(shot.clone())));
        let c = ctx(fake);
        let target = DisplayTarget::Docker {
            container: "c".into(),
            protocol: DisplayProtocol::X11,
            display: None,
        };
        let op = DisplayOp::Screenshot {
            region: Some(Region {
                x: 0,
                y: 0,
                w: 20,
                h: 10,
            }),
            scale: Some(0.5),
        };
        let out = dispatch(&c, &target, &op).await.unwrap();
        assert_eq!(out.value["width"], 10);
        assert_eq!(out.value["height"], 5);
        assert_eq!(out.value["screen"]["width"], 40);
        let png = out.png.unwrap();
        assert_eq!(image::dims(&png).unwrap(), (10, 5));
        assert_eq!(out.value["sha256"], sha256(&png));
    }

    #[tokio::test]
    async fn bad_values_never_reach_a_tool() {
        let fake = FakeRunner::ok();
        let c = ctx(fake.clone());
        let target = DisplayTarget::Docker {
            container: "c".into(),
            protocol: DisplayProtocol::X11,
            display: None,
        };
        let e = dispatch(&c, &target, &DisplayOp::Move { x: -5, y: 1 })
            .await
            .unwrap_err();
        assert_eq!(e.code(), ErrorCode::BadRequest);
        let e = dispatch(
            &c,
            &target,
            &DisplayOp::Keys {
                chord: "ctrl-flurb".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::BadRequest);
        assert!(fake.lines().is_empty());
    }
}
