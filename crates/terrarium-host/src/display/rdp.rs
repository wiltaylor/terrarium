//! An RDP client for the `rdp` display driver, on IronRDP.
//!
//! - **Security**: TLS, and NLA (CredSSP, NTLM) when the server offers it;
//!   TLS alone (the server shows its own login, and the client logs in
//!   with the Client Info PDU) when it does not. The client does not speak
//!   standard RDP security (RC4) with no TLS.
//! - **TLS**: rustls on ring (no OpenSSL). The client accepts the
//!   server's self-signed certificate on the first connection and pins its
//!   SHA-256 in [`PinStore`] (trust on first use). A later connection with
//!   another certificate fails before the client sends the credentials.
//! - **Screen**: a task for each connection reads the server's updates
//!   into a framebuffer all the time (RDP pushes them). IronRDP decodes
//!   the updates: RemoteFX (lossy) and plain bitmaps. A screenshot waits
//!   until the updates stop for a moment, then copies the framebuffer.
//! - **Input**: fast-path mouse events (move, buttons, wheel), set-1
//!   scancodes for chords ([`chord_scancodes`]), and Unicode key events
//!   for text.
//!
//! The password goes into the connector config only. No error text holds
//! it.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ironrdp::connector::{self, ClientConnector, ClientConnectorState, Credentials};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::nego::SecurityProtocol;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{FramedWrite as _, TokioFramed};
use terrarium_core::display::{Button, Key, KeyChord, Modifier};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use super::image::Image;
use super::{DResult, DisplayError};

/// The desktop size the client asks for. The server may pick another.
const DESKTOP: connector::DesktopSize = connector::DesktopSize {
    width: 1280,
    height: 800,
};
/// A screenshot waits until the screen had no update for this long.
const SETTLE: Duration = Duration::from_millis(250);
/// ... but no longer than this after the login ...
const FIRST_WAIT: Duration = Duration::from_secs(4);
/// ... or this for a screenshot.
const LATER_WAIT: Duration = Duration::from_millis(1500);
/// One wheel step in RDP rotation units.
const WHEEL_STEP: i16 = 120;

// ---------------------------------------------------------------- pins

/// The pinned TLS certificates of RDP servers: one `<address> <sha256>`
/// line for each server, in the host agent's state
/// (`[display] rdp_known_hosts`).
#[derive(Debug)]
pub struct PinStore {
    path: PathBuf,
    lock: Mutex<()>,
}

/// What [`PinStore::check`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// No pin yet: the store has the certificate now.
    New,
    /// The pinned certificate.
    Same,
}

impl PinStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> std::io::Result<Vec<(String, String)>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| {
                let (a, f) = l.split_once(char::is_whitespace)?;
                Some((a.to_owned(), f.trim().to_owned()))
            })
            .collect())
    }

    /// Check the certificate of `address` against its pin, or pin it.
    pub fn check(&self, address: &str, fingerprint: &str) -> DResult<Pin> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let failed = |e: std::io::Error| {
            DisplayError::Failed(format!(
                "rdp {address}: cannot use the certificate pins in {}: {e}",
                self.path.display()
            ))
        };
        let pins = self.read().map_err(failed)?;
        if let Some((_, want)) = pins.iter().find(|(a, _)| a == address) {
            if want.eq_ignore_ascii_case(fingerprint) {
                return Ok(Pin::Same);
            }
            return Err(DisplayError::Refused(format!(
                "rdp {address}: the server's TLS certificate changed: it has SHA-256 \
                 {fingerprint}, and the host agent pinned {want} on the first connection. \
                 Another machine may be in the middle. If the server has a new certificate on \
                 purpose, remove the pin on the host agent's machine with `terrarium-host \
                 rdp-forget {address}` (or delete its line in {}), then connect again",
                self.path.display()
            )));
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(failed)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(failed)?;
        writeln!(f, "{address} {fingerprint}").map_err(failed)?;
        tracing::info!(address, fingerprint, path = %self.path.display(), "rdp: pinned the server certificate");
        Ok(Pin::New)
    }

    /// Remove the pin of `address`. False when it had none.
    pub fn forget(&self, address: &str) -> std::io::Result<bool> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let pins = self.read()?;
        let keep: Vec<_> = pins.iter().filter(|(a, _)| a != address).collect();
        if keep.len() == pins.len() {
            return Ok(false);
        }
        let mut text = String::new();
        for (a, f) in keep {
            text.push_str(&format!("{a} {f}\n"));
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(true)
    }

    /// The pins, for `terrarium-host check`.
    pub fn list(&self) -> std::io::Result<Vec<(String, String)>> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read()
    }
}

// ----------------------------------------------------------- scancodes

/// The set-1 scancode of a modifier (left key).
pub fn modifier_scancode(m: Modifier) -> Scancode {
    match m {
        Modifier::Ctrl => Scancode::from_u8(false, 0x1d),
        Modifier::Alt => Scancode::from_u8(false, 0x38),
        Modifier::Shift => Scancode::from_u8(false, 0x2a),
        Modifier::Super => Scancode::from_u8(true, 0x5b),
    }
}

/// The set-1 scancode of a key (US layout), with the extended (E0) flag.
/// `None` for a key that has no plain scancode (pause).
pub fn key_scancode(k: &Key) -> Option<Scancode> {
    const LETTERS: [u8; 26] = [
        0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
    ];
    let n = |c| Some(Scancode::from_u8(false, c));
    let e = |c| Some(Scancode::from_u8(true, c));
    match k {
        Key::Char(c @ 'a'..='z') => n(LETTERS[(*c as u8 - b'a') as usize]),
        Key::Char('0') => n(0x0b),
        Key::Char(c @ '1'..='9') => n(0x02 + (*c as u8 - b'1')),
        Key::Char(_) => None,
        Key::F(f @ 1..=10) => n(0x3a + f),
        Key::F(11) => n(0x57),
        Key::F(12) => n(0x58),
        Key::F(f @ 13..=23) => n(0x64 + (f - 13)),
        Key::F(24) => n(0x76),
        Key::F(_) => None,
        Key::Named(name) => match name.as_str() {
            "enter" => n(0x1c),
            "tab" => n(0x0f),
            "backspace" => n(0x0e),
            "space" => n(0x39),
            "escape" => n(0x01),
            "delete" => e(0x53),
            "insert" => e(0x52),
            "up" => e(0x48),
            "down" => e(0x50),
            "left" => e(0x4b),
            "right" => e(0x4d),
            "home" => e(0x47),
            "end" => e(0x4f),
            "pageup" => e(0x49),
            "pagedown" => e(0x51),
            "menu" => e(0x5d),
            "print" => e(0x37),
            "capslock" => n(0x3a),
            "numlock" => n(0x45),
            "scrolllock" => n(0x46),
            "minus" => n(0x0c),
            "equal" => n(0x0d),
            "bracketleft" => n(0x1a),
            "bracketright" => n(0x1b),
            "backslash" => n(0x2b),
            "semicolon" => n(0x27),
            "apostrophe" => n(0x28),
            "comma" => n(0x33),
            "period" => n(0x34),
            "slash" => n(0x35),
            "grave" => n(0x29),
            _ => None,
        },
    }
}

/// The scancodes of a chord: the modifiers in order, then the key.
pub fn chord_scancodes(chord: &KeyChord) -> DResult<Vec<Scancode>> {
    let key = key_scancode(&chord.key).ok_or_else(|| {
        DisplayError::Unsupported(format!(
            "the rdp display has no scancode for the key `{}`",
            chord.key.name()
        ))
    })?;
    let mut v: Vec<Scancode> = chord
        .modifiers
        .iter()
        .map(|m| modifier_scancode(*m))
        .collect();
    v.push(key);
    Ok(v)
}

/// The RDP mouse button.
pub fn mouse_button(b: Button) -> MouseButton {
    match b {
        Button::Left => MouseButton::Left,
        Button::Middle => MouseButton::Middle,
        Button::Right => MouseButton::Right,
    }
}

/// The wheel operations of `dx`, `dy` steps: one for each step. A
/// positive `dy` scrolls down (negative rotation); a positive `dx` scrolls
/// right.
pub fn wheel_steps(dx: i32, dy: i32) -> Vec<Operation> {
    let step = |vertical: bool, units: i16| {
        Operation::WheelRotations(WheelRotations {
            is_vertical: vertical,
            rotation_units: units,
        })
    };
    let mut v = Vec::new();
    for _ in 0..dy.unsigned_abs() {
        v.push(step(true, if dy > 0 { -WHEEL_STEP } else { WHEEL_STEP }));
    }
    for _ in 0..dx.unsigned_abs() {
        v.push(step(false, if dx > 0 { WHEEL_STEP } else { -WHEEL_STEP }));
    }
    v
}

/// The operations that type one character: newline and tab as keys,
/// anything else as a Unicode key event.
pub fn char_ops(c: char) -> [Operation; 2] {
    match c {
        '\n' | '\r' => [
            Operation::KeyPressed(Scancode::from_u8(false, 0x1c)),
            Operation::KeyReleased(Scancode::from_u8(false, 0x1c)),
        ],
        '\t' => [
            Operation::KeyPressed(Scancode::from_u8(false, 0x0f)),
            Operation::KeyReleased(Scancode::from_u8(false, 0x0f)),
        ],
        c => [
            Operation::UnicodeKeyPressed(c),
            Operation::UnicodeKeyReleased(c),
        ],
    }
}

// ----------------------------------------------------------------- DER

/// Read one DER TLV at `b`: (tag, contents, rest).
fn der(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, b) = b.split_first()?;
    let (&l0, mut b) = b.split_first()?;
    let len = if l0 < 0x80 {
        usize::from(l0)
    } else {
        let n = usize::from(l0 & 0x7f);
        if n == 0 || n > 4 || b.len() < n {
            return None;
        }
        let mut len = 0usize;
        for &x in &b[..n] {
            len = (len << 8) | usize::from(x);
        }
        b = &b[n..];
        len
    };
    if b.len() < len {
        return None;
    }
    Some((tag, &b[..len], &b[len..]))
}

/// The subject public key of a DER certificate: the contents of the BIT
/// STRING in SubjectPublicKeyInfo, with no unused-bits byte. CredSSP binds
/// to it.
pub fn spki_public_key(cert: &[u8]) -> Option<Vec<u8>> {
    let (0x30, certificate, _) = der(cert)? else {
        return None;
    };
    let (0x30, tbs, _) = der(certificate)? else {
        return None;
    };
    let mut rest = tbs;
    // [0] version is optional.
    let (tag, _, r) = der(rest)?;
    if tag == 0xa0 {
        rest = r;
    }
    // serial, signature, issuer, validity, subject.
    for _ in 0..5 {
        rest = der(rest)?.2;
    }
    let (0x30, spki, _) = der(rest)? else {
        return None;
    };
    let (_, _, after_alg) = der(spki)?;
    let (0x03, bits, _) = der(after_alg)? else {
        return None;
    };
    let (&unused, key) = bits.split_first()?;
    (unused == 0).then(|| key.to_vec())
}

// ---------------------------------------------------------- connection

/// The framebuffer that the connection task keeps.
struct Frame {
    image: DecodedImage,
    updates: u64,
    last: Instant,
    /// Why the connection ended.
    ended: Option<String>,
}

enum Cmd {
    Input(Vec<Operation>, oneshot::Sender<Result<(), String>>),
}

/// One input step.
#[derive(Debug, Clone)]
pub enum Ev {
    Ops(Vec<Operation>),
    /// Wait some milliseconds, so that the server sees each step.
    Pause(u64),
}

/// One connection to an RDP server. Dropping it ends the connection.
pub struct RdpConn {
    address: String,
    frame: Arc<Mutex<Frame>>,
    tx: mpsc::Sender<Cmd>,
    pub width: u16,
    pub height: u16,
    /// `nla` or `tls`.
    pub security: &'static str,
    /// The pin: new on the first connection.
    pub pin: Pin,
}

/// The credentials of an RDP target. No `Debug`: it holds the password.
pub struct RdpLogin<'a> {
    pub username: &'a str,
    pub domain: Option<&'a str>,
    pub password: &'a str,
}

/// No Kerberos: NTLM, the norm for a user name and a password, needs no
/// KDC.
struct NoKerberos;

impl ironrdp_tokio::NetworkClient for NoKerberos {
    async fn send(
        &mut self,
        _request: &connector::sspi::generator::NetworkRequest,
    ) -> connector::ConnectorResult<Vec<u8>> {
        Err(connector::general_err!(
            "Kerberos is not supported; use NTLM"
        ))
    }
}

fn config(login: &RdpLogin<'_>) -> connector::Config {
    // `DOMAIN\user` gives the domain when the target has none.
    let (domain, username) = match (login.domain, login.username.split_once('\\')) {
        (Some(d), _) => (Some(d.to_owned()), login.username.to_owned()),
        (None, Some((d, u))) => (Some(d.to_owned()), u.to_owned()),
        (None, None) => (None, login.username.to_owned()),
    };
    connector::Config {
        credentials: Credentials::UsernamePassword {
            username,
            password: login.password.to_owned(),
        },
        domain,
        enable_tls: true,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0x0409,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DESKTOP,
        desktop_scale_factor: 0,
        // IronRDP's default codecs: RemoteFX, and plain bitmaps. RemoteFX
        // is lossy (faint marks at the 64-pixel tile corners), but the
        // plain-bitmap path drew xrdp's screen wrong when the client
        // offered no RemoteFX (docs/compat.md).
        bitmap: None,
        client_build: 0,
        client_name: "terrarium".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        autologon: true,
        enable_audio_playback: false,
        performance_flags: PerformanceFlags::default(),
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: None,
        enable_server_pointer: false,
        pointer_software_rendering: true,
        multitransport_flags: None,
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

/// The host of `host:port` or `[v6]:port`.
fn host_of(address: &str) -> &str {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    address.rsplit_once(':').map_or(address, |(h, _)| h)
}

impl RdpConn {
    /// Connect and log in. `timeout` limits all of it.
    pub async fn connect(
        address: &str,
        login: RdpLogin<'_>,
        pins: &PinStore,
        timeout: Duration,
    ) -> DResult<Self> {
        match tokio::time::timeout(timeout, Self::connect_inner(address, login, pins)).await {
            Ok(r) => r,
            Err(_) => Err(DisplayError::Failed(format!(
                "rdp {address}: no login in {} s",
                timeout.as_secs()
            ))),
        }
    }

    async fn connect_inner(address: &str, login: RdpLogin<'_>, pins: &PinStore) -> DResult<Self> {
        let fail = |what: &str, e: &dyn std::fmt::Display| {
            DisplayError::Failed(format!("rdp {address}: {what}: {e}"))
        };
        let host = host_of(address).to_owned();
        let tcp = TcpStream::connect(address)
            .await
            .map_err(|e| fail("connect", &e))?;
        let _ = tcp.set_nodelay(true);
        let client_addr = tcp.local_addr().map_err(|e| fail("connect", &e))?;

        let mut framed = TokioFramed::new(tcp);
        let mut conn = ClientConnector::new(config(&login), client_addr);
        let upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut conn)
            .await
            .map_err(|e| {
                fail(
                    "negotiation (the client needs TLS or NLA; standard RDP security alone is \
                     not supported)",
                    &e,
                )
            })?;
        let security = match &conn.state {
            ClientConnectorState::EnhancedSecurityUpgrade { selected_protocol }
                if selected_protocol
                    .intersects(SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX) =>
            {
                "nla"
            }
            _ => "tls",
        };

        // TLS, then the pin, before any credential goes out.
        let tcp = framed.into_inner_no_leftover();
        let tls_cfg = crate::tls::any_cert_config().map_err(|e| fail("tls", &e))?;
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| fail("tls server name", &e))?;
        let tls = tokio_rustls::TlsConnector::from(tls_cfg)
            .connect(name, tcp)
            .await
            .map_err(|e| fail("tls", &e))?;
        let cert = tls
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .map(|c| c.as_ref().to_vec())
            .ok_or_else(|| fail("tls", &"the server sent no certificate"))?;
        let pin = pins.check(address, &crate::tls::fingerprint_sha256(&cert))?;
        let public_key = spki_public_key(&cert)
            .ok_or_else(|| fail("tls", &"cannot read the public key of the certificate"))?;

        let upgraded = ironrdp_tokio::mark_as_upgraded(upgrade, &mut conn);
        let mut framed = TokioFramed::new(tls);
        let result = ironrdp_tokio::connect_finalize(
            upgraded,
            conn,
            &mut framed,
            &mut NoKerberos,
            connector::ServerName::new(host),
            public_key,
            None,
        )
        .await
        .map_err(|e| {
            fail(
                "login (check the target's user name, domain, and credential)",
                &e,
            )
        })?;

        let (width, height) = (result.desktop_size.width, result.desktop_size.height);
        let stage = ActiveStageBuilder {
            static_channels: result.static_channels,
            user_channel_id: result.user_channel_id,
            io_channel_id: result.io_channel_id,
            message_channel_id: result.message_channel_id,
            share_id: result.share_id,
            compression_type: result.compression_type,
            enable_server_pointer: result.enable_server_pointer,
            pointer_software_rendering: result.pointer_software_rendering,
        }
        .build();
        let frame = Arc::new(Mutex::new(Frame {
            image: DecodedImage::new(PixelFormat::RgbA32, width, height),
            updates: 0,
            last: Instant::now(),
            ended: None,
        }));
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(pump(address.to_owned(), framed, stage, frame.clone(), rx));
        let c = Self {
            address: address.to_owned(),
            frame,
            tx,
            width,
            height,
            security,
            pin,
        };
        // xrdp drops the input that comes before the session draws its
        // first screen: wait for the first updates.
        c.settle(FIRST_WAIT).await?;
        Ok(c)
    }

    fn fail(&self, m: impl std::fmt::Display) -> DisplayError {
        DisplayError::Failed(format!("rdp {}: {m}", self.address))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Frame> {
        self.frame.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// An error when the connection ended.
    pub fn alive(&self) -> DResult<()> {
        match &self.lock().ended {
            Some(why) => Err(self.fail(why)),
            None => Ok(()),
        }
    }

    /// Wait until the screen updates stop for a moment ([`SETTLE`]), at
    /// most `wait`.
    async fn settle(&self, wait: Duration) -> DResult<()> {
        let start = Instant::now();
        loop {
            {
                let f = self.lock();
                if let Some(why) = &f.ended {
                    return Err(self.fail(why));
                }
                let quiet = f.last.elapsed() >= SETTLE && start.elapsed() >= SETTLE;
                if (f.updates > 0 && quiet) || start.elapsed() >= wait {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A PNG of the screen, once the updates stop for a moment.
    pub async fn screenshot(&mut self) -> DResult<Vec<u8>> {
        self.settle(LATER_WAIT).await?;
        let img = {
            let f = self.lock();
            let (w, h) = (f.image.width(), f.image.height());
            let (stride, data) = (f.image.stride(), f.image.data());
            let mut rgba = Vec::with_capacity(usize::from(w) * usize::from(h) * 4);
            for row in 0..usize::from(h) {
                let line = &data[row * stride..row * stride + usize::from(w) * 4];
                for p in line.chunks_exact(4) {
                    rgba.extend_from_slice(&[p[0], p[1], p[2], 0xff]);
                }
            }
            Image::from_rgba(u32::from(w), u32::from(h), rgba)?
        };
        img.encode()
    }

    /// Send the input steps.
    pub async fn input(&self, evs: Vec<Ev>) -> DResult<()> {
        for ev in evs {
            match ev {
                Ev::Pause(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                Ev::Ops(ops) => {
                    let (tx, rx) = oneshot::channel();
                    self.tx
                        .send(Cmd::Input(ops, tx))
                        .await
                        .map_err(|_| self.fail("the connection ended"))?;
                    rx.await
                        .map_err(|_| self.fail("the connection ended"))?
                        .map_err(|e| self.fail(e))?;
                }
            }
        }
        Ok(())
    }

    /// A move that the screen bounds clamp.
    pub fn move_op(&self, x: i32, y: i32) -> Operation {
        let c = |v: i32, side: u16| v.clamp(0, i32::from(side.saturating_sub(1))) as u16;
        Operation::MouseMove(MousePosition {
            x: c(x, self.width),
            y: c(y, self.height),
        })
    }
}

type Framed = TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>;

/// The connection task: read the server's PDUs into the framebuffer, and
/// send the input that comes on `rx`. It ends when `rx` closes (the
/// connection was dropped) or the server ends the session.
async fn pump(
    address: String,
    mut framed: Framed,
    mut stage: ActiveStage,
    frame: Arc<Mutex<Frame>>,
    mut rx: mpsc::Receiver<Cmd>,
) {
    let lock = || frame.lock().unwrap_or_else(|e| e.into_inner());
    let mut db = Database::new();
    let why = loop {
        tokio::select! {
            pdu = framed.read_pdu() => {
                let (action, payload) = match pdu {
                    Ok(v) => v,
                    Err(e) => break format!("the server closed the connection ({e})"),
                };
                let outputs = {
                    let mut f = lock();
                    stage.process(&mut f.image, action, &payload)
                };
                match outputs {
                    Ok(o) => {
                        if let Err(why) = outputs_done(&mut framed, &frame, o).await {
                            break why;
                        }
                    }
                    Err(e) => break format!("session error: {e}"),
                }
            }
            cmd = rx.recv() => {
                let Some(Cmd::Input(ops, reply)) = cmd else {
                    // Dropped: log off politely.
                    if let Ok(outs) = stage.graceful_shutdown() {
                        for o in outs {
                            if let ActiveStageOutput::ResponseFrame(b) = o {
                                let _ = framed.write_all(&b).await;
                            }
                        }
                    }
                    tracing::debug!(address, "rdp: connection closed");
                    return;
                };
                let events = db.apply(ops);
                let outputs = {
                    let mut f = lock();
                    stage.process_fastpath_input(&mut f.image, &events)
                };
                let res = match outputs {
                    Ok(o) => outputs_done(&mut framed, &frame, o).await,
                    Err(e) => Err(format!("input error: {e}")),
                };
                let failed = res.clone().err();
                let _ = reply.send(res);
                if let Some(why) = failed {
                    break why;
                }
            }
        }
    };
    tracing::debug!(address, why, "rdp: connection ended");
    lock().ended = Some(why);
}

/// Act on the outputs of the session: send the replies, and note the
/// screen updates. An error ends the connection.
async fn outputs_done(
    framed: &mut Framed,
    frame: &Mutex<Frame>,
    outputs: Vec<ActiveStageOutput>,
) -> Result<(), String> {
    for out in outputs {
        match out {
            ActiveStageOutput::ResponseFrame(b) => framed
                .write_all(&b)
                .await
                .map_err(|e| format!("write: {e}"))?,
            ActiveStageOutput::GraphicsUpdate(_) => {
                let mut f = frame.lock().unwrap_or_else(|e| e.into_inner());
                f.updates += 1;
                f.last = Instant::now();
            }
            ActiveStageOutput::Terminate(reason) => {
                return Err(format!("the server ended the session: {reason}"));
            }
            ActiveStageOutput::DeactivateAll => {
                return Err(
                    "the server reset the session (a new screen size?); the next operation \
                     connects again"
                        .into(),
                );
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancodes_of_chords() {
        let sc = |c: &str| {
            chord_scancodes(&KeyChord::parse(c).unwrap())
                .unwrap()
                .into_iter()
                .map(|s| s.as_u16())
                .collect::<Vec<_>>()
        };
        assert_eq!(sc("ctrl-alt-delete"), [0x1d, 0x38, 0xe053]);
        assert_eq!(sc("super-l"), [0xe05b, 0x26]);
        assert_eq!(sc("shift-f10"), [0x2a, 0x44]);
        assert_eq!(sc("enter"), [0x1c]);
        assert_eq!(sc("a"), [0x1e]);
        assert_eq!(sc("z"), [0x2c]);
        assert_eq!(sc("q"), [0x10]);
        assert_eq!(sc("m"), [0x32]);
        assert_eq!(sc("1"), [0x02]);
        assert_eq!(sc("9"), [0x0a]);
        assert_eq!(sc("0"), [0x0b]);
        assert_eq!(sc("f1"), [0x3b]);
        assert_eq!(sc("f11"), [0x57]);
        assert_eq!(sc("f13"), [0x64]);
        assert_eq!(sc("f24"), [0x76]);
        assert_eq!(sc("pagedown"), [0xe051]);
        assert_eq!(sc("up"), [0xe048]);
        assert_eq!(sc("ctrl-/"), [0x1d, 0x35]);
        assert_eq!(sc("grave"), [0x29]);
        let e = chord_scancodes(&KeyChord::parse("pause").unwrap()).unwrap_err();
        assert!(e.to_string().contains("no scancode"), "{e}");
        // Every key name but pause has a scancode.
        for name in [
            "tab",
            "backspace",
            "space",
            "escape",
            "insert",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "menu",
            "print",
            "capslock",
            "numlock",
            "scrolllock",
            "minus",
            "equal",
            "[",
            "]",
            "\\",
            ";",
            "'",
            ",",
            ".",
        ] {
            assert!(
                chord_scancodes(&KeyChord::parse(name).unwrap()).is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn text_and_wheel_operations() {
        assert!(matches!(
            char_ops('é')[0],
            Operation::UnicodeKeyPressed('é')
        ));
        assert!(matches!(
            char_ops('é')[1],
            Operation::UnicodeKeyReleased('é')
        ));
        assert!(matches!(char_ops('\n')[0], Operation::KeyPressed(s) if s.as_u16() == 0x1c));
        let w = wheel_steps(1, 2);
        assert_eq!(w.len(), 3);
        let units: Vec<(bool, i16)> = w
            .iter()
            .map(|o| match o {
                Operation::WheelRotations(r) => (r.is_vertical, r.rotation_units),
                _ => panic!("{o:?}"),
            })
            .collect();
        assert_eq!(units, [(true, -120), (true, -120), (false, 120)]);
        assert!(
            matches!(&wheel_steps(0, -1)[0], Operation::WheelRotations(r) if r.rotation_units == 120)
        );
    }

    #[test]
    fn pins_trust_on_first_use_and_refuse_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = PinStore::new(dir.path().join("sub/rdp-known-hosts"));
        assert_eq!(store.check("win:3389", "AA:BB").unwrap(), Pin::New);
        assert_eq!(store.check("win:3389", "aa:bb").unwrap(), Pin::Same);
        assert_eq!(store.check("other:3389", "CC").unwrap(), Pin::New);
        let e = store.check("win:3389", "DD:EE").unwrap_err();
        assert_eq!(e.code(), crate::protocol::ErrorCode::Refused);
        let m = e.to_string();
        assert!(m.contains("terrarium-host rdp-forget win:3389"), "{m}");
        assert!(m.contains("DD:EE") && m.contains("AA:BB"), "{m}");
        assert_eq!(store.list().unwrap().len(), 2);
        assert!(store.forget("win:3389").unwrap());
        assert!(!store.forget("win:3389").unwrap());
        assert_eq!(store.check("win:3389", "DD:EE").unwrap(), Pin::New);
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert_eq!(text, "other:3389 CC\nwin:3389 DD:EE\n");
    }

    #[test]
    fn the_public_key_comes_out_of_a_certificate() {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["win.lab".to_owned()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        let got = spki_public_key(cert.der()).unwrap();
        assert_eq!(got, key.public_key_raw());
        assert!(spki_public_key(&cert.der()[..40]).is_none());
        assert!(spki_public_key(b"").is_none());
    }

    #[test]
    fn hosts_of_addresses() {
        assert_eq!(host_of("10.0.0.5:3389"), "10.0.0.5");
        assert_eq!(host_of("[fe80::1]:3389"), "fe80::1");
        assert_eq!(host_of("win"), "win");
    }

    #[test]
    fn the_domain_comes_from_the_user_name_when_the_target_has_none() {
        let c = config(&RdpLogin {
            username: "LAB\\bob",
            domain: None,
            password: "pw",
        });
        assert_eq!(c.domain.as_deref(), Some("LAB"));
        assert!(
            matches!(&c.credentials, Credentials::UsernamePassword { username, .. } if username == "bob")
        );
        let c = config(&RdpLogin {
            username: "bob",
            domain: Some("CORP"),
            password: "pw",
        });
        assert_eq!(c.domain.as_deref(), Some("CORP"));
    }
}
