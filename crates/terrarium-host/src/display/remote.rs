//! The VNC and RDP drivers, and the connections that they keep.
//!
//! The protocol clients are in [`super::vnc`] (RFB) and [`super::rdp`]
//! (IronRDP). This module keeps one connection for each target in the
//! host agent ([`Remotes`]): a live view takes a screenshot every 2.5 s,
//! and a new login each time is slow (and, for RDP, noisy on the server).
//!
//! - **Key**: the address and a SHA-256 of the user name, the domain, and
//!   the password. A new credential gets a new connection. The key holds
//!   no secret, and nothing logs it.
//! - **One operation at a time**: a driver holds its connection's lock for
//!   the whole operation, so two operations on one target do not mix
//!   their input.
//! - **Idle timeout**: a connection with no operation for
//!   [`IDLE_TIMEOUT`] closes. A task checks every few seconds.
//! - **Failures**: an operation that fails drops the connection. When the
//!   connection was an old one, the driver connects again and tries the
//!   operation once more (the server may have closed an idle link).
//!
//! The password comes only in the `display` frame; never log it
//! ([`DisplayTarget`]'s `Debug` hides it), and no error text holds it.
//!
//! [`DisplayTarget`]: crate::protocol::DisplayTarget

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use terrarium_core::display::{Button, KeyChord};
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::rdp::{self, PinStore, RdpConn, RdpLogin};
use super::vnc::{self, VncConn};
use super::{DResult, DisplayCtx, DisplayDriver, DisplayError, Features};

/// A connection with no operation for this long closes.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest connect and login.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// The pause between two input steps, so that the server sees each.
const STEP_MS: u64 = 15;
/// The pause between a press and a release of a mouse button.
const CLICK_MS: u64 = 40;

/// The default VNC port.
pub const VNC_PORT: u16 = 5900;
/// The default RDP port.
pub const RDP_PORT: u16 = 3389;

/// Check `host:port` (or `host`, or `[v6]:port`) and add the default port.
pub fn address(addr: &str, default_port: u16) -> DResult<String> {
    let bad = || {
        DisplayError::BadRequest(format!(
            "bad address {addr:?}: use host:port, for example 10.0.0.5:{default_port}"
        ))
    };
    if addr.is_empty()
        || addr.len() > 255
        || addr.starts_with('-')
        || addr
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '@')
    {
        return Err(bad());
    }
    let (host, port) = if let Some(rest) = addr.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or_else(bad)?;
        match after {
            "" => (h.to_owned(), default_port),
            p => (
                h.to_owned(),
                p.strip_prefix(':')
                    .and_then(|p| p.parse().ok())
                    .ok_or_else(bad)?,
            ),
        }
    } else {
        match addr.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h.to_owned(), p.parse().map_err(|_| bad())?),
            Some(_) => return Err(bad()),
            None => (addr.to_owned(), default_port),
        }
    };
    if host.is_empty() || port == 0 {
        return Err(bad());
    }
    Ok(if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    })
}

/// A TCP connect to `address` (`host:port`, checked by [`address`]):
/// `{"address", "reachable", "elapsed_ms", "error"?}`.
pub async fn tcp_probe(addr: &str, timeout: std::time::Duration) -> DResult<serde_json::Value> {
    // The daemon sends the address with its port; 0 means "no default".
    let addr = address(addr, 0)?;
    let start = std::time::Instant::now();
    let res = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let error = match res {
        Ok(Ok(_)) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(_) => Some(format!("no answer in {} s", timeout.as_secs())),
    };
    let mut v = serde_json::json!({
        "address": addr, "reachable": error.is_none(), "elapsed_ms": elapsed_ms,
    });
    if let Some(e) = error {
        v["error"] = serde_json::json!(e);
    }
    Ok(v)
}

/// One connection slot: the connection, and when it was last used.
pub struct Slot<C> {
    conn: Option<C>,
    used: Instant,
}

type Slots<C> = StdMutex<HashMap<[u8; 32], Arc<Mutex<Slot<C>>>>>;

/// The VNC and RDP connections of the host agent, and the RDP certificate
/// pins. One for each host agent ([`DisplayCtx::remotes`]).
pub struct Remotes {
    vnc: Slots<VncConn>,
    rdp: Slots<RdpConn>,
    pub pins: PinStore,
    idle: Duration,
    reaper: OnceLock<()>,
}

impl Remotes {
    /// `pins`: the file of the RDP certificate pins.
    pub fn new(pins: std::path::PathBuf) -> Arc<Self> {
        Self::with_idle(pins, IDLE_TIMEOUT)
    }

    pub fn with_idle(pins: std::path::PathBuf, idle: Duration) -> Arc<Self> {
        Arc::new(Self {
            vnc: StdMutex::new(HashMap::new()),
            rdp: StdMutex::new(HashMap::new()),
            pins: PinStore::new(pins),
            idle,
            reaper: OnceLock::new(),
        })
    }

    /// The open connections: (vnc, rdp).
    pub fn open(&self) -> (usize, usize) {
        fn count<C>(s: &Slots<C>) -> usize {
            s.lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .filter(|v| v.try_lock().map_or(true, |g| g.conn.is_some()))
                .count()
        }
        (count(&self.vnc), count(&self.rdp))
    }

    fn slot<C: Send + 'static>(
        self: &Arc<Self>,
        slots: impl Fn(&Self) -> &Slots<C>,
        key: [u8; 32],
    ) -> Arc<Mutex<Slot<C>>> {
        self.reaper.get_or_init(|| {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(reap(Arc::downgrade(self)));
            }
        });
        slots(self)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert_with(|| {
                Arc::new(Mutex::new(Slot {
                    conn: None,
                    used: Instant::now(),
                }))
            })
            .clone()
    }

    /// Close the idle connections, and forget the empty slots.
    fn sweep(&self) {
        fn sweep_one<C>(s: &Slots<C>, idle: Duration) {
            s.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, v| {
                // A slot in use is not idle.
                let Ok(mut g) = v.try_lock() else {
                    return true;
                };
                if g.conn.is_some() && g.used.elapsed() >= idle {
                    g.conn = None;
                }
                g.conn.is_some()
            });
        }
        sweep_one(&self.vnc, self.idle);
        sweep_one(&self.rdp, self.idle);
    }
}

async fn reap(remotes: Weak<Remotes>) {
    let every = remotes.upgrade().map_or(Duration::from_secs(15), |r| {
        (r.idle / 4).max(Duration::from_millis(100))
    });
    loop {
        tokio::time::sleep(every).await;
        let Some(r) = remotes.upgrade() else {
            return;
        };
        r.sweep();
    }
}

/// The slot key: the address, and a hash of the credential.
fn key(kind: &str, address: &str, parts: &[Option<&str>]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(kind.as_bytes());
    h.update([0]);
    h.update(address.as_bytes());
    for p in parts {
        h.update([0, u8::from(p.is_some())]);
        h.update(p.unwrap_or_default().as_bytes());
    }
    h.finalize().into()
}

/// Run `exec` on the slot's connection: connect when there is none (or it
/// is idle or dead), drop it on a failure, and try once more on a fresh
/// connection when an old one failed.
macro_rules! with_conn {
    ($self:ident, $slot:ident, $check:expr, $connect:expr, |$c:ident| $exec:expr) => {{
        let mut $slot = $self.slot.lock().await;
        let $slot = &mut **$slot;
        if $slot.used.elapsed() >= $self.idle {
            $slot.conn = None;
        }
        if let Some($c) = $slot.conn.as_mut() {
            if let Err(e) = $check {
                tracing::debug!(backend = $self.backend(), error = %e, "display: drop a dead connection");
                $slot.conn = None;
            }
        }
        let mut fresh = false;
        let result = loop {
            let $c = match $slot.conn.as_mut() {
                Some(c) => c,
                None => {
                    fresh = true;
                    $slot.conn.insert($connect.await?)
                }
            };
            match $exec.await {
                Ok(v) => break Ok(v),
                Err(e) => {
                    $slot.conn = None;
                    if fresh || !matches!(e, DisplayError::Failed(_)) {
                        break Err(e);
                    }
                    tracing::debug!(backend = $self.backend(), error = %e, "display: connect again");
                }
            }
        };
        $slot.used = Instant::now();
        result
    }};
}

/// The input steps of a click at `at` (or where the pointer is).
fn click_steps<E>(
    at: Option<(i32, i32)>,
    double: bool,
    mv: impl Fn(i32, i32) -> E,
    down: impl Fn() -> E,
    up: impl Fn() -> E,
    pause: impl Fn(u64) -> E,
) -> Vec<E> {
    let mut v = Vec::new();
    if let Some((x, y)) = at {
        v.push(mv(x, y));
        v.push(pause(STEP_MS));
    }
    for i in 0..if double { 2 } else { 1 } {
        if i > 0 {
            v.push(pause(80));
        }
        v.push(down());
        v.push(pause(CLICK_MS));
        v.push(up());
    }
    v
}

/// The points of a drag from `from` to `to`, in ten steps.
fn drag_points(from: (i32, i32), to: (i32, i32)) -> Vec<(i32, i32)> {
    (1..=10)
        .map(|i| {
            (
                from.0 + (to.0 - from.0) * i / 10,
                from.1 + (to.1 - from.1) * i / 10,
            )
        })
        .collect()
}

// ----------------------------------------------------------------- VNC

/// A VNC server.
pub struct VncDriver {
    pub address: String,
    password: Option<String>,
    timeout: Duration,
    idle: Duration,
    slot: Mutex<OwnedMutexGuard<Slot<VncConn>>>,
}

impl VncDriver {
    /// The driver of a target, on its kept connection. It holds the
    /// connection's lock until it drops. The VNC protocol has no user
    /// name: `username` is not used.
    pub async fn open(
        ctx: &DisplayCtx,
        addr: &str,
        username: Option<&str>,
        password: Option<String>,
    ) -> DResult<Self> {
        let address = address(addr, VNC_PORT)?;
        let k = key("vnc", &address, &[username, password.as_deref()]);
        let slot = ctx.remotes.slot(|r| &r.vnc, k).lock_owned().await;
        Ok(Self {
            address,
            password,
            timeout: ctx.timeout,
            idle: ctx.remotes.idle,
            slot: Mutex::new(slot),
        })
    }

    async fn input(&self, evs: &[vnc::Ev]) -> DResult<()> {
        with_conn!(
            self,
            slot,
            c.drain(self.timeout).await,
            VncConn::connect(
                &self.address,
                self.password.as_deref(),
                self.timeout.min(CONNECT_TIMEOUT)
            ),
            |c| c.input(evs)
        )
    }
}

impl DisplayDriver for VncDriver {
    fn backend(&self) -> &'static str {
        "vnc"
    }

    fn features(&self) -> Features {
        Features {
            scroll: true,
            ..Features::default()
        }
    }

    async fn screenshot(&self) -> DResult<Vec<u8>> {
        with_conn!(
            self,
            slot,
            c.drain(self.timeout).await,
            VncConn::connect(
                &self.address,
                self.password.as_deref(),
                self.timeout.min(CONNECT_TIMEOUT)
            ),
            |c| c.screenshot(self.timeout)
        )
    }

    async fn move_to(&self, x: i32, y: i32) -> DResult<()> {
        self.input(&[vnc::Ev::Move(x, y)]).await
    }

    async fn click(&self, at: Option<(i32, i32)>, button: Button, double: bool) -> DResult<()> {
        let m = vnc::button_mask(button);
        let steps = click_steps(
            at,
            double,
            vnc::Ev::Move,
            || vnc::Ev::Mask(m),
            || vnc::Ev::Mask(0),
            vnc::Ev::Pause,
        );
        self.input(&steps).await
    }

    async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> DResult<()> {
        let mut v = vec![
            vnc::Ev::Move(from.0, from.1),
            vnc::Ev::Pause(STEP_MS),
            vnc::Ev::Mask(1),
        ];
        for (x, y) in drag_points(from, to) {
            v.push(vnc::Ev::Pause(STEP_MS));
            v.push(vnc::Ev::Move(x, y));
        }
        v.push(vnc::Ev::Pause(STEP_MS));
        v.push(vnc::Ev::Mask(0));
        self.input(&v).await
    }

    async fn scroll(&self, at: Option<(i32, i32)>, dx: i32, dy: i32) -> DResult<()> {
        let mut v = Vec::new();
        if let Some((x, y)) = at {
            v.push(vnc::Ev::Move(x, y));
        }
        // Bit 3 up, 4 down, 5 left, 6 right: a press and a release each step.
        let vertical = if dy > 0 { 1 << 4 } else { 1 << 3 };
        let horizontal = if dx > 0 { 1 << 6 } else { 1 << 5 };
        for (n, bit) in [(dy, vertical), (dx, horizontal)] {
            for _ in 0..n.unsigned_abs() {
                v.extend([
                    vnc::Ev::Mask(bit),
                    vnc::Ev::Mask(0),
                    vnc::Ev::Pause(STEP_MS),
                ]);
            }
        }
        self.input(&v).await
    }

    async fn keys(&self, chord: &KeyChord) -> DResult<()> {
        let mods: Vec<u32> = chord.modifiers.iter().map(|m| m.x_keysym_value()).collect();
        let key = chord.key.x_keysym_value();
        let mut v = Vec::new();
        for m in &mods {
            v.push(vnc::Ev::Key(*m, true));
            v.push(vnc::Ev::Pause(STEP_MS));
        }
        v.extend([
            vnc::Ev::Key(key, true),
            vnc::Ev::Pause(CLICK_MS),
            vnc::Ev::Key(key, false),
        ]);
        for m in mods.iter().rev() {
            v.push(vnc::Ev::Pause(STEP_MS));
            v.push(vnc::Ev::Key(*m, false));
        }
        self.input(&v).await
    }

    async fn type_text(&self, text: &str) -> DResult<()> {
        let mut v = Vec::new();
        for c in text.chars() {
            let k = vnc::char_keysym(c);
            v.extend([
                vnc::Ev::Key(k, true),
                vnc::Ev::Key(k, false),
                vnc::Ev::Pause(5),
            ]);
        }
        self.input(&v).await
    }
}

// ----------------------------------------------------------------- RDP

/// An RDP server.
pub struct RdpDriver {
    pub address: String,
    username: Option<String>,
    domain: Option<String>,
    password: Option<String>,
    timeout: Duration,
    idle: Duration,
    remotes: Arc<Remotes>,
    slot: Mutex<OwnedMutexGuard<Slot<RdpConn>>>,
}

impl RdpDriver {
    /// The driver of a target, on its kept connection. It holds the
    /// connection's lock until it drops.
    pub async fn open(
        ctx: &DisplayCtx,
        addr: &str,
        username: Option<String>,
        domain: Option<String>,
        password: Option<String>,
    ) -> DResult<Self> {
        let address = address(addr, RDP_PORT)?;
        let k = key(
            "rdp",
            &address,
            &[username.as_deref(), domain.as_deref(), password.as_deref()],
        );
        let slot = ctx.remotes.slot(|r| &r.rdp, k).lock_owned().await;
        Ok(Self {
            address,
            username,
            domain,
            password,
            timeout: ctx.timeout,
            idle: ctx.remotes.idle,
            remotes: ctx.remotes.clone(),
            slot: Mutex::new(slot),
        })
    }

    async fn connect(&self) -> DResult<RdpConn> {
        let (Some(username), Some(password)) = (&self.username, &self.password) else {
            return Err(DisplayError::BadRequest(
                "an rdp target needs a user name and a credential (the password)".into(),
            ));
        };
        let c = RdpConn::connect(
            &self.address,
            RdpLogin {
                username,
                domain: self.domain.as_deref(),
                password,
            },
            &self.remotes.pins,
            self.timeout.min(CONNECT_TIMEOUT),
        )
        .await?;
        tracing::info!(
            address = self.address,
            security = c.security,
            width = c.width,
            height = c.height,
            "rdp: connected"
        );
        Ok(c)
    }

    async fn input(&self, evs: impl Fn(&RdpConn) -> Vec<rdp::Ev>) -> DResult<()> {
        with_conn!(self, slot, c.alive(), self.connect(), |c| c.input(evs(c)))
    }
}

/// Input operations in steps, with a pause after each.
fn rdp_steps(ops: impl IntoIterator<Item = ironrdp::input::Operation>, ms: u64) -> Vec<rdp::Ev> {
    let mut v = Vec::new();
    for op in ops {
        v.push(rdp::Ev::Ops(vec![op]));
        v.push(rdp::Ev::Pause(ms));
    }
    v
}

impl DisplayDriver for RdpDriver {
    fn backend(&self) -> &'static str {
        "rdp"
    }

    fn features(&self) -> Features {
        Features {
            scroll: true,
            ..Features::default()
        }
    }

    async fn screenshot(&self) -> DResult<Vec<u8>> {
        with_conn!(self, slot, c.alive(), self.connect(), |c| c.screenshot())
    }

    async fn move_to(&self, x: i32, y: i32) -> DResult<()> {
        self.input(|c| vec![rdp::Ev::Ops(vec![c.move_op(x, y)])])
            .await
    }

    async fn click(&self, at: Option<(i32, i32)>, button: Button, double: bool) -> DResult<()> {
        use ironrdp::input::Operation as Op;
        let b = rdp::mouse_button(button);
        self.input(|c| {
            click_steps(
                at,
                double,
                |x, y| rdp::Ev::Ops(vec![c.move_op(x, y)]),
                || rdp::Ev::Ops(vec![Op::MouseButtonPressed(b)]),
                || rdp::Ev::Ops(vec![Op::MouseButtonReleased(b)]),
                rdp::Ev::Pause,
            )
        })
        .await
    }

    async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> DResult<()> {
        use ironrdp::input::{MouseButton, Operation as Op};
        self.input(|c| {
            let mut v = vec![
                rdp::Ev::Ops(vec![c.move_op(from.0, from.1)]),
                rdp::Ev::Pause(STEP_MS),
                rdp::Ev::Ops(vec![Op::MouseButtonPressed(MouseButton::Left)]),
            ];
            for (x, y) in drag_points(from, to) {
                v.push(rdp::Ev::Pause(STEP_MS));
                v.push(rdp::Ev::Ops(vec![c.move_op(x, y)]));
            }
            v.push(rdp::Ev::Pause(STEP_MS));
            v.push(rdp::Ev::Ops(vec![Op::MouseButtonReleased(
                MouseButton::Left,
            )]));
            v
        })
        .await
    }

    async fn scroll(&self, at: Option<(i32, i32)>, dx: i32, dy: i32) -> DResult<()> {
        self.input(|c| {
            let mut v = Vec::new();
            if let Some((x, y)) = at {
                v.push(rdp::Ev::Ops(vec![c.move_op(x, y)]));
            }
            v.extend(rdp_steps(rdp::wheel_steps(dx, dy), STEP_MS));
            v
        })
        .await
    }

    async fn keys(&self, chord: &KeyChord) -> DResult<()> {
        use ironrdp::input::Operation as Op;
        let codes = rdp::chord_scancodes(chord)?;
        let ops = codes
            .iter()
            .map(|s| Op::KeyPressed(*s))
            .chain(codes.iter().rev().map(|s| Op::KeyReleased(*s)));
        let steps = rdp_steps(ops, STEP_MS);
        self.input(|_| steps.clone()).await
    }

    async fn type_text(&self, text: &str) -> DResult<()> {
        let ops = text.chars().flat_map(rdp::char_ops);
        let steps = rdp_steps(ops, 5);
        self.input(|_| steps.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::dispatch;
    use super::super::runner::FakeRunner;
    use super::super::testing::ctx;
    use super::*;
    use crate::protocol::{DisplayTarget, ErrorCode};
    use terrarium_core::display::DisplayOp;

    #[test]
    fn addresses_get_the_default_port() {
        assert_eq!(address("10.0.0.5", 5900).unwrap(), "10.0.0.5:5900");
        assert_eq!(address("win:3390", 3389).unwrap(), "win:3390");
        assert_eq!(address("[::1]", 5900).unwrap(), "[::1]:5900");
        assert_eq!(address("[fe80::1]:5901", 5900).unwrap(), "[fe80::1]:5901");
        for bad in ["", "-x", "a b", "h:0", "h:x", "::1", "user@h", "h/1"] {
            assert!(address(bad, 5900).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn the_probe_connects_or_says_why_not() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let v = tcp_probe(&addr, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(v["reachable"], true, "{v}");
        drop(l);
        // Port 1 on loopback refuses at once. A freed port can be taken
        // again by a parallel test.
        let v = tcp_probe("127.0.0.1:1", std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(v["reachable"], false, "{v}");
        assert!(v["error"].is_string());
        assert!(
            tcp_probe("no-port", std::time::Duration::from_secs(1))
                .await
                .is_err()
        );
    }

    use super::super::vnc::fake::{FakeServer, Got};

    fn vnc(address: &str, password: Option<&str>) -> DisplayTarget {
        DisplayTarget::Vnc {
            address: address.into(),
            username: None,
            password: password.map(str::to_owned),
        }
    }

    fn keys(got: &[Got]) -> Vec<(bool, u32)> {
        got.iter()
            .filter_map(|g| match g {
                Got::Key { down, sym } => Some((*down, *sym)),
                Got::Pointer { .. } => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn vnc_ops_go_through_the_engine_on_one_connection() {
        let s = FakeServer::start(64, 48, Some("pw"), true).await;
        let fake = FakeRunner::ok();
        let c = ctx(fake.clone());
        let t = vnc(&s.address, Some("pw"));

        let out = dispatch(&c, &t, &DisplayOp::Info).await.unwrap();
        assert_eq!(out.value["backend"], "vnc");
        assert_eq!(out.value["width"], 64);
        assert_eq!(out.value["height"], 48);
        let ops = out.value["ops"].as_array().unwrap();
        assert!(ops.iter().any(|o| o == "scroll"), "{ops:?}");

        let shot = DisplayOp::Screenshot {
            region: None,
            scale: None,
        };
        let out = dispatch(&c, &t, &shot).await.unwrap();
        let img = super::super::image::Image::decode(&out.png.unwrap()).unwrap();
        let [r, g, b] = super::super::vnc::fake::pixel(10, 20);
        let i = (20 * 64 + 10) * 4;
        assert_eq!(&img.rgba[i..i + 4], &[r, g, b, 255]);

        // A chord: modifiers down in order, the key, then up in reverse.
        dispatch(
            &c,
            &t,
            &DisplayOp::Keys {
                chord: "ctrl-alt-t".into(),
            },
        )
        .await
        .unwrap();
        // Text: one keysym for each character.
        dispatch(
            &c,
            &t,
            &DisplayOp::Type {
                text: "aZ é€\n".into(),
            },
        )
        .await
        .unwrap();
        dispatch(&c, &t, &shot).await.unwrap();
        let got = s.take();
        assert_eq!(
            keys(&got),
            [
                (true, 0xffe3),
                (true, 0xffe9),
                (true, 0x74),
                (false, 0x74),
                (false, 0xffe9),
                (false, 0xffe3),
                (true, 0x61),
                (false, 0x61),
                (true, 0x5a),
                (false, 0x5a),
                (true, 0x20),
                (false, 0x20),
                (true, 0xe9),
                (false, 0xe9),
                (true, 0x0100_20ac),
                (false, 0x0100_20ac),
                (true, 0xff0d),
                (false, 0xff0d),
            ]
        );

        // Scroll: 2 down, 1 left: a press and a release of bit 4, twice,
        // then bit 5, at the point.
        dispatch(
            &c,
            &t,
            &DisplayOp::Scroll {
                x: Some(5),
                y: Some(6),
                dx: -1,
                dy: 2,
            },
        )
        .await
        .unwrap();
        // A double right click where the pointer is, and a drag.
        dispatch(
            &c,
            &t,
            &DisplayOp::Click {
                x: None,
                y: None,
                button: Button::Right,
                double: true,
            },
        )
        .await
        .unwrap();
        dispatch(
            &c,
            &t,
            &DisplayOp::Drag {
                x1: 1,
                y1: 1,
                x2: 11,
                y2: 21,
            },
        )
        .await
        .unwrap();
        dispatch(&c, &t, &shot).await.unwrap();
        let got = s.take();
        let p = |mask, x, y| Got::Pointer { mask, x, y };
        assert_eq!(
            &got[..9],
            &[
                p(0, 5, 6),
                p(16, 5, 6),
                p(0, 5, 6),
                p(16, 5, 6),
                p(0, 5, 6),
                p(32, 5, 6),
                p(0, 5, 6),
                p(4, 5, 6),
                p(0, 5, 6),
            ]
        );
        assert_eq!(&got[9..11], &[p(4, 5, 6), p(0, 5, 6)]);
        assert_eq!(&got[11..13], &[p(0, 1, 1), p(1, 1, 1)]);
        assert_eq!(got.last(), Some(&p(0, 11, 21)));
        assert_eq!(got[got.len() - 2], p(1, 11, 21));

        // All of it on one connection.
        assert_eq!(*s.sessions.lock().unwrap(), 1);
        assert_eq!(c.remotes.open(), (1, 0));
        assert!(fake.lines().is_empty(), "no tool ran");
    }

    #[tokio::test]
    async fn a_new_password_is_a_new_connection_and_errors_hide_it() {
        let s = FakeServer::start(8, 8, Some("good"), false).await;
        let c = ctx(FakeRunner::ok());
        let e = dispatch(
            &c,
            &vnc(&s.address, Some("bad-secret-77")),
            &DisplayOp::Info,
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::Failed);
        assert!(!e.to_string().contains("bad-secret-77"), "{e}");
        dispatch(&c, &vnc(&s.address, Some("good")), &DisplayOp::Info)
            .await
            .unwrap();
        assert_eq!(*s.sessions.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn idle_connections_close_and_dead_ones_reconnect() {
        let s = FakeServer::start(8, 8, None, false).await;
        let mut c = ctx(FakeRunner::ok());
        c.remotes = Remotes::with_idle(
            std::env::temp_dir().join("terrarium-test-pins-unused"),
            Duration::from_millis(200),
        );
        let t = vnc(&s.address, None);
        dispatch(&c, &t, &DisplayOp::Info).await.unwrap();
        assert_eq!(c.remotes.open(), (1, 0));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(c.remotes.open(), (0, 0), "the reaper closed it");
        dispatch(&c, &t, &DisplayOp::Move { x: 1, y: 1 })
            .await
            .unwrap();
        assert_eq!(*s.sessions.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn rdp_needs_a_user_and_a_password_before_it_connects() {
        let c = ctx(FakeRunner::ok());
        let t = DisplayTarget::Rdp {
            address: "127.0.0.1:1".into(),
            username: None,
            domain: None,
            password: Some("pw".into()),
        };
        let e = dispatch(&c, &t, &DisplayOp::Info).await.unwrap_err();
        assert_eq!(e.code(), ErrorCode::BadRequest);
        let t = DisplayTarget::Rdp {
            address: "127.0.0.1:1".into(),
            username: Some("bob".into()),
            domain: None,
            password: Some("pw-xyz-123".into()),
        };
        let e = dispatch(&c, &t, &DisplayOp::Info).await.unwrap_err();
        assert_eq!(e.code(), ErrorCode::Failed);
        assert!(e.to_string().contains("rdp 127.0.0.1:1: connect"), "{e}");
        assert!(!e.to_string().contains("pw-xyz-123"));
        let e = dispatch(
            &c,
            &t,
            &DisplayOp::Keys {
                chord: "pause".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::Unsupported);
    }

    #[tokio::test]
    async fn bad_addresses_never_connect() {
        let fake = FakeRunner::ok();
        let c = ctx(fake.clone());
        let bad = DisplayTarget::Vnc {
            address: "-oProxyCommand".into(),
            username: None,
            password: None,
        };
        let e = dispatch(&c, &bad, &DisplayOp::Info).await.unwrap_err();
        assert_eq!(e.code(), ErrorCode::BadRequest);
        assert!(fake.lines().is_empty());
    }
}
