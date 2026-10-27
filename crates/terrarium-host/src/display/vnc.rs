//! A VNC (RFB) client for the `vnc` display driver: the handshake, the
//! framebuffer, and the input messages (RFC 6143).
//!
//! - **Versions**: the client speaks 3.8, and 3.7 or 3.3 to an older
//!   server.
//! - **Security**: type 2 (VNC authentication: DES on a 16-byte challenge,
//!   with the password as the key) and type 1 (none). The client has no
//!   TLS (VeNCrypt): the screen and the keys cross the link in clear text.
//!   Keep the link on a trusted network, or in an SSH tunnel or a VPN.
//! - **Pixels**: the client asks for 32-bit true colour, little-endian,
//!   red at bit 16: each pixel is the bytes B, G, R, X.
//! - **Encodings**: ZRLE (zlib, one stream for each connection), CopyRect,
//!   Raw, and the DesktopSize pseudo-encoding (the server resized).
//! - **Updates**: each screenshot asks for a full, not incremental, update
//!   and reads the one update that answers it. The server sends no update
//!   that the client did not ask for, so the link is quiet between
//!   operations.
//!
//! The password goes into the DES key only. No error text holds it.

use std::time::Duration;

use flate2::{Decompress, FlushDecompress};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use super::image::Image;
use super::{DResult, DisplayError};

/// The largest screen side the client takes.
const MAX_SIDE: u16 = 8192;
/// The largest compressed or uncompressed ZRLE rectangle.
const MAX_ZRLE_BYTES: usize = 256 << 20;
/// The largest cut text the client skips.
const MAX_CUT_TEXT: u32 = 16 << 20;
/// The most DesktopSize changes that one screenshot follows.
const MAX_RESIZES: u32 = 4;

const ENC_RAW: i32 = 0;
const ENC_COPY_RECT: i32 = 1;
const ENC_ZRLE: i32 = 16;
const ENC_DESKTOP_SIZE: i32 = -223;

/// One input step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ev {
    /// Move the pointer (the buttons in the current mask stay down).
    Move(i32, i32),
    /// Set the button mask at the current position: bit 0 left, 1 middle,
    /// 2 right, 3 and 4 wheel up and down, 5 and 6 wheel left and right.
    Mask(u8),
    /// Press (`true`) or release a key, by X keysym.
    Key(u32, bool),
    /// Wait some milliseconds, so that the server sees each step.
    Pause(u64),
}

/// The button mask bit of a mouse button.
pub fn button_mask(b: terrarium_core::display::Button) -> u8 {
    1 << (b.x_number() - 1)
}

/// The keysym of one typed character: Latin-1 is the code point, other
/// Unicode is `0x0100_0000 + code point`; newline and tab are Return and
/// Tab.
pub fn char_keysym(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        c if (' '..='~').contains(&c) || ('\u{a0}'..='\u{ff}').contains(&c) => c as u32,
        c => 0x0100_0000 + c as u32,
    }
}

/// Reverse the bits of each byte: the VNC form of a DES key.
fn vnc_key(password: &[u8]) -> [u8; 8] {
    let mut key = [0u8; 8];
    for (k, p) in key.iter_mut().zip(password) {
        *k = p.reverse_bits();
    }
    key
}

/// The answer to a VNC authentication challenge: each 8-byte half
/// encrypted with DES, with the first 8 bytes of the password (bits
/// reversed) as the key.
pub fn vnc_auth_response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    use des::Des;
    use des::cipher::{Block, BlockCipherEncrypt, KeyInit};
    let cipher = Des::new(&vnc_key(password).into());
    let mut out = [0u8; 16];
    for (i, half) in challenge.chunks_exact(8).enumerate() {
        let mut block = Block::<Des>::default();
        block.copy_from_slice(half);
        cipher.encrypt_block(&mut block);
        out[i * 8..i * 8 + 8].copy_from_slice(&block);
    }
    out
}

/// One connection to a VNC server.
pub struct VncConn {
    rd: BufReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
    address: String,
    pub width: u16,
    pub height: u16,
    /// BGRX, `width * height * 4` bytes.
    fb: Vec<u8>,
    zlib: Decompress,
    pos: (u16, u16),
    mask: u8,
    /// The protocol version, `3.3`, `3.7`, or `3.8`.
    pub version: &'static str,
    /// `vnc` (type 2) or `none` (type 1).
    pub security: &'static str,
    /// The desktop name from ServerInit.
    pub name: String,
}

fn io_err(addr: &str, what: &str) -> impl Fn(std::io::Error) -> DisplayError + use<> {
    let (addr, what) = (addr.to_owned(), what.to_owned());
    move |e| DisplayError::Failed(format!("vnc {addr}: {what}: {e}"))
}

impl VncConn {
    /// Connect, do the handshake, and set the pixel format and the
    /// encodings. `timeout` limits all of it.
    pub async fn connect(
        address: &str,
        password: Option<&str>,
        timeout: Duration,
    ) -> DResult<Self> {
        match tokio::time::timeout(timeout, Self::connect_inner(address, password)).await {
            Ok(r) => r,
            Err(_) => Err(DisplayError::Failed(format!(
                "vnc {address}: no handshake in {} s",
                timeout.as_secs()
            ))),
        }
    }

    async fn connect_inner(address: &str, password: Option<&str>) -> DResult<Self> {
        let tcp = TcpStream::connect(address)
            .await
            .map_err(io_err(address, "connect"))?;
        let _ = tcp.set_nodelay(true);
        let (r, w) = tcp.into_split();
        let mut rd = BufReader::new(r);
        let mut wr = w;
        let hs = io_err(address, "handshake");
        let fail = |m: String| DisplayError::Failed(format!("vnc {address}: {m}"));

        let mut ver = [0u8; 12];
        rd.read_exact(&mut ver).await.map_err(&hs)?;
        let minor = parse_version(&ver).ok_or_else(|| {
            fail(format!(
                "not an RFB server (it said {:?})",
                String::from_utf8_lossy(&ver)
            ))
        })?;
        let (version, reply): (&'static str, &[u8; 12]) = match minor {
            m if m >= 8 => ("3.8", b"RFB 003.008\n"),
            7 => ("3.7", b"RFB 003.007\n"),
            _ => ("3.3", b"RFB 003.003\n"),
        };
        wr.write_all(reply).await.map_err(&hs)?;

        // The security types: a list from 3.7, one u32 the server picks
        // in 3.3. 0 is a refusal with a reason.
        let types: Vec<u8> = if version == "3.3" {
            match rd.read_u32().await.map_err(&hs)? {
                0 => return Err(fail(format!("refused: {}", read_reason(&mut rd).await?))),
                t => vec![u8::try_from(t).unwrap_or(0)],
            }
        } else {
            let n = rd.read_u8().await.map_err(&hs)?;
            if n == 0 {
                return Err(fail(format!("refused: {}", read_reason(&mut rd).await?)));
            }
            let mut t = vec![0u8; usize::from(n)];
            rd.read_exact(&mut t).await.map_err(&hs)?;
            t
        };
        let pick = match password {
            Some(_) if types.contains(&2) => 2,
            _ if types.contains(&1) => 1,
            None if types.contains(&2) => {
                return Err(DisplayError::Failed(format!(
                    "vnc {address}: the server asks for a password and the target has none: \
                     set the target's credential"
                )));
            }
            _ => {
                return Err(fail(format!(
                    "the server offers security types {types:?}; the client speaks only 2 (VNC \
                     authentication) and 1 (none), not TLS or VeNCrypt: set the server to VNC \
                     authentication and keep the link on a trusted network or a tunnel"
                )));
            }
        };
        if version != "3.3" {
            wr.write_u8(pick).await.map_err(&hs)?;
        }
        let security = if pick == 2 {
            let mut challenge = [0u8; 16];
            rd.read_exact(&mut challenge).await.map_err(&hs)?;
            let pw = password.unwrap_or_default().as_bytes();
            wr.write_all(&vnc_auth_response(pw, &challenge))
                .await
                .map_err(&hs)?;
            "vnc"
        } else {
            "none"
        };
        // SecurityResult: always in 3.8; after authentication only before.
        if (version == "3.8" || pick == 2) && rd.read_u32().await.map_err(&hs)? != 0 {
            let why = if version == "3.8" {
                read_reason(&mut rd).await.unwrap_or_default()
            } else {
                String::new()
            };
            return Err(DisplayError::Failed(format!(
                "vnc {address}: authentication failed{}: check the target's credential",
                if why.is_empty() {
                    String::new()
                } else {
                    format!(" ({why})")
                }
            )));
        }

        // ClientInit: shared, so that other viewers stay on.
        wr.write_u8(1).await.map_err(&hs)?;
        let width = rd.read_u16().await.map_err(&hs)?;
        let height = rd.read_u16().await.map_err(&hs)?;
        let mut pf = [0u8; 16];
        rd.read_exact(&mut pf).await.map_err(&hs)?;
        let name_len = rd.read_u32().await.map_err(&hs)?;
        let mut name = vec![0u8; name_len.min(4096) as usize];
        rd.read_exact(&mut name).await.map_err(&hs)?;
        if name_len > 4096 {
            skip(&mut rd, u64::from(name_len) - 4096)
                .await
                .map_err(&hs)?;
        }
        check_size(address, width, height)?;

        // SetPixelFormat: 32 bpp, depth 24, little-endian, true colour,
        // red at 16, green at 8, blue at 0.
        let mut msg = vec![0u8, 0, 0, 0, 32, 24, 0, 1];
        msg.extend_from_slice(&255u16.to_be_bytes());
        msg.extend_from_slice(&255u16.to_be_bytes());
        msg.extend_from_slice(&255u16.to_be_bytes());
        msg.extend_from_slice(&[16, 8, 0, 0, 0, 0]);
        // SetEncodings.
        let encs = [ENC_ZRLE, ENC_COPY_RECT, ENC_RAW, ENC_DESKTOP_SIZE];
        msg.extend_from_slice(&[2, 0]);
        msg.extend_from_slice(&(encs.len() as u16).to_be_bytes());
        for e in encs {
            msg.extend_from_slice(&e.to_be_bytes());
        }
        wr.write_all(&msg).await.map_err(&hs)?;

        Ok(Self {
            rd,
            wr,
            address: address.to_owned(),
            width,
            height,
            fb: vec![0; usize::from(width) * usize::from(height) * 4],
            zlib: Decompress::new(true),
            pos: (0, 0),
            mask: 0,
            version,
            security,
            name: String::from_utf8_lossy(&name).into_owned(),
        })
    }

    fn fail(&self, m: impl std::fmt::Display) -> DisplayError {
        DisplayError::Failed(format!("vnc {}: {m}", self.address))
    }

    fn io(&self) -> impl Fn(std::io::Error) -> DisplayError + use<> {
        io_err(&self.address, "read")
    }

    /// Read the server messages that wait on the link, with no wait for
    /// more. An error when the server closed the link.
    pub async fn drain(&mut self, timeout: Duration) -> DResult<()> {
        loop {
            match tokio::time::timeout(Duration::ZERO, self.rd.fill_buf()).await {
                Err(_) => return Ok(()),
                Ok(Err(e)) => return Err(self.io()(e)),
                Ok(Ok([])) => return Err(self.fail("the server closed the connection")),
                Ok(Ok(_)) => {
                    let t = self.rd.read_u8().await.map_err(self.io())?;
                    match tokio::time::timeout(timeout, self.message(t)).await {
                        Ok(r) => {
                            r?;
                        }
                        Err(_) => return Err(self.fail("a message did not arrive in full")),
                    }
                }
            }
        }
    }

    /// A PNG of the full screen: ask for a full update and read it.
    pub async fn screenshot(&mut self, timeout: Duration) -> DResult<Vec<u8>> {
        match tokio::time::timeout(timeout, self.update()).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(self.fail(format!("no framebuffer update in {} s", timeout.as_secs())));
            }
        }
        Image::from_bgrx(u32::from(self.width), u32::from(self.height), &self.fb)?.encode()
    }

    async fn update(&mut self) -> DResult<()> {
        let mut resizes = 0;
        loop {
            self.request_full().await?;
            loop {
                let t = self.rd.read_u8().await.map_err(self.io())?;
                if t == 0 {
                    break;
                }
                self.message(t).await?;
            }
            if !self.read_update().await? {
                return Ok(());
            }
            resizes += 1;
            if resizes > MAX_RESIZES {
                return Err(self.fail("the screen size keeps changing"));
            }
        }
    }

    async fn request_full(&mut self) -> DResult<()> {
        let mut m = vec![3u8, 0, 0, 0, 0, 0];
        m.extend_from_slice(&self.width.to_be_bytes());
        m.extend_from_slice(&self.height.to_be_bytes());
        self.wr
            .write_all(&m)
            .await
            .map_err(io_err(&self.address, "write"))
    }

    /// One server message other than an update, after its type byte.
    async fn message(&mut self, t: u8) -> DResult<()> {
        let io = self.io();
        match t {
            0 => {
                self.read_update().await?;
            }
            // SetColourMapEntries: pad, first, count, then 6 bytes each.
            1 => {
                self.rd.read_u8().await.map_err(&io)?;
                self.rd.read_u16().await.map_err(&io)?;
                let n = self.rd.read_u16().await.map_err(&io)?;
                skip(&mut self.rd, u64::from(n) * 6).await.map_err(&io)?;
            }
            // Bell.
            2 => {}
            // ServerCutText: 3 pad, length, text.
            3 => {
                let mut pad = [0u8; 3];
                self.rd.read_exact(&mut pad).await.map_err(&io)?;
                let n = self.rd.read_u32().await.map_err(&io)?;
                if n > MAX_CUT_TEXT {
                    return Err(self.fail(format!("cut text of {n} bytes")));
                }
                skip(&mut self.rd, u64::from(n)).await.map_err(&io)?;
            }
            t => return Err(self.fail(format!("unknown server message type {t}"))),
        }
        Ok(())
    }

    /// A FramebufferUpdate after its type byte. True when the screen size
    /// changed: the pixels are then not all there.
    async fn read_update(&mut self) -> DResult<bool> {
        let io = self.io();
        self.rd.read_u8().await.map_err(&io)?;
        let n = self.rd.read_u16().await.map_err(&io)?;
        let mut resized = false;
        let mut encodings: Vec<i32> = Vec::new();
        for _ in 0..n {
            let x = self.rd.read_u16().await.map_err(&io)?;
            let y = self.rd.read_u16().await.map_err(&io)?;
            let w = self.rd.read_u16().await.map_err(&io)?;
            let h = self.rd.read_u16().await.map_err(&io)?;
            let enc = self.rd.read_i32().await.map_err(&io)?;
            if !encodings.contains(&enc) {
                encodings.push(enc);
            }
            if enc == ENC_DESKTOP_SIZE {
                check_size(&self.address, w, h)?;
                self.width = w;
                self.height = h;
                self.fb = vec![0; usize::from(w) * usize::from(h) * 4];
                resized = true;
                continue;
            }
            if u32::from(x) + u32::from(w) > u32::from(self.width)
                || u32::from(y) + u32::from(h) > u32::from(self.height)
            {
                return Err(self.fail(format!(
                    "a rectangle {w}x{h}+{x}+{y} is outside the {}x{} screen",
                    self.width, self.height
                )));
            }
            let r = Rect { x, y, w, h };
            match enc {
                ENC_RAW => {
                    let row = usize::from(w) * 4;
                    let mut buf = vec![0u8; row * usize::from(h)];
                    self.rd.read_exact(&mut buf).await.map_err(&io)?;
                    for (i, line) in buf.chunks_exact(row.max(1)).enumerate() {
                        let off = self.offset(x, y + i as u16);
                        self.fb[off..off + row].copy_from_slice(line);
                    }
                }
                ENC_COPY_RECT => {
                    let sx = self.rd.read_u16().await.map_err(&io)?;
                    let sy = self.rd.read_u16().await.map_err(&io)?;
                    self.copy_rect(sx, sy, r)?;
                }
                ENC_ZRLE => {
                    let len = self.rd.read_u32().await.map_err(&io)? as usize;
                    if len > MAX_ZRLE_BYTES {
                        return Err(self.fail(format!("a ZRLE rectangle of {len} bytes")));
                    }
                    let mut data = vec![0u8; len];
                    self.rd.read_exact(&mut data).await.map_err(&io)?;
                    let raw = inflate(&mut self.zlib, &data).map_err(|e| self.fail(e))?;
                    zrle(&mut self.fb, self.width, r, &raw).map_err(|e| self.fail(e))?;
                }
                e => return Err(self.fail(format!("the server sent encoding {e}, not asked for"))),
            }
        }
        tracing::trace!(address = self.address, rects = n, ?encodings, "vnc: update");
        Ok(resized)
    }

    fn offset(&self, x: u16, y: u16) -> usize {
        (usize::from(y) * usize::from(self.width) + usize::from(x)) * 4
    }

    fn copy_rect(&mut self, sx: u16, sy: u16, r: Rect) -> DResult<()> {
        if u32::from(sx) + u32::from(r.w) > u32::from(self.width)
            || u32::from(sy) + u32::from(r.h) > u32::from(self.height)
        {
            return Err(self.fail("a CopyRect source is outside the screen"));
        }
        let row = usize::from(r.w) * 4;
        let mut tmp = Vec::with_capacity(row * usize::from(r.h));
        for i in 0..r.h {
            let off = self.offset(sx, sy + i);
            tmp.extend_from_slice(&self.fb[off..off + row]);
        }
        for (i, line) in tmp.chunks_exact(row.max(1)).enumerate() {
            let off = self.offset(r.x, r.y + i as u16);
            self.fb[off..off + row].copy_from_slice(line);
        }
        Ok(())
    }

    /// Send the input steps.
    pub async fn input(&mut self, evs: &[Ev]) -> DResult<()> {
        let mut buf = Vec::new();
        for ev in evs {
            match *ev {
                Ev::Move(x, y) => {
                    self.pos = (clamp(x, self.width), clamp(y, self.height));
                    self.pointer_msg(&mut buf);
                }
                Ev::Mask(m) => {
                    self.mask = m;
                    self.pointer_msg(&mut buf);
                }
                Ev::Key(sym, down) => {
                    buf.extend_from_slice(&[4, u8::from(down), 0, 0]);
                    buf.extend_from_slice(&sym.to_be_bytes());
                }
                Ev::Pause(ms) => {
                    self.flush(&mut buf).await?;
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
            }
        }
        self.flush(&mut buf).await
    }

    fn pointer_msg(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&[5, self.mask]);
        buf.extend_from_slice(&self.pos.0.to_be_bytes());
        buf.extend_from_slice(&self.pos.1.to_be_bytes());
    }

    async fn flush(&mut self, buf: &mut Vec<u8>) -> DResult<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.wr
            .write_all(buf)
            .await
            .map_err(io_err(&self.address, "write"))?;
        buf.clear();
        Ok(())
    }
}

fn clamp(v: i32, side: u16) -> u16 {
    v.clamp(0, i32::from(side.saturating_sub(1))) as u16
}

fn check_size(address: &str, w: u16, h: u16) -> DResult<()> {
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(DisplayError::Failed(format!(
            "vnc {address}: the server has a {w}x{h} screen; the client takes 1 to {MAX_SIDE} \
             pixels a side"
        )));
    }
    Ok(())
}

/// `RFB 003.008\n` gives 8. `None` for anything else than RFB 3.x.
fn parse_version(v: &[u8; 12]) -> Option<u32> {
    let s = std::str::from_utf8(v).ok()?;
    let rest = s.strip_prefix("RFB 003.")?.strip_suffix('\n')?;
    rest.parse().ok()
}

async fn read_reason(rd: &mut BufReader<OwnedReadHalf>) -> DResult<String> {
    let io = |e: std::io::Error| DisplayError::Failed(format!("vnc: read the reason: {e}"));
    let n = rd.read_u32().await.map_err(io)?;
    let mut b = vec![0u8; n.min(4096) as usize];
    rd.read_exact(&mut b).await.map_err(io)?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

async fn skip(rd: &mut BufReader<OwnedReadHalf>, n: u64) -> std::io::Result<()> {
    let copied = tokio::io::copy(&mut (&mut *rd).take(n), &mut tokio::io::sink()).await?;
    if copied < n {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

/// Inflate the zlib data of one ZRLE rectangle with the connection's
/// stream.
fn inflate(z: &mut Decompress, data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::with_capacity(data.len().saturating_mul(4).max(4096));
    let mut input = data;
    loop {
        if out.capacity() - out.len() < 4096 {
            out.reserve(out.capacity().max(64 * 1024));
        }
        let (in0, out0) = (z.total_in(), z.total_out());
        z.decompress_vec(input, &mut out, FlushDecompress::Sync)
            .map_err(|e| format!("zlib: {e}"))?;
        let used = (z.total_in() - in0) as usize;
        let made = z.total_out() - out0;
        input = &input[used..];
        if out.len() > MAX_ZRLE_BYTES {
            return Err("a ZRLE rectangle inflates past the limit".into());
        }
        // Done when the input is used up and the output has room left.
        if input.is_empty() && out.len() < out.capacity() {
            return Ok(out);
        }
        if used == 0 && made == 0 {
            return Err("zlib: the stream does not move".into());
        }
    }
}

/// A reader over inflated ZRLE data.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cur<'_> {
    fn u8(&mut self) -> Result<u8, String> {
        let v = *self.b.get(self.i).ok_or("ZRLE data ends early")?;
        self.i += 1;
        Ok(v)
    }

    /// A CPIXEL: 3 bytes, B G R.
    fn cpixel(&mut self) -> Result<[u8; 4], String> {
        let s = self
            .b
            .get(self.i..self.i + 3)
            .ok_or("ZRLE data ends early")?;
        self.i += 3;
        Ok([s[0], s[1], s[2], 0])
    }

    /// A run length: bytes of 255 add up, the last byte is under 255; the
    /// run is the sum plus one.
    fn run(&mut self) -> Result<usize, String> {
        let mut n = 1usize;
        loop {
            let b = self.u8()?;
            n += usize::from(b);
            if b != 255 {
                return Ok(n);
            }
            if n > 64 * 64 {
                return Err("a ZRLE run is longer than a tile".into());
            }
        }
    }
}

/// Decode ZRLE tiles (64x64, rows first) into the framebuffer.
fn zrle(fb: &mut [u8], fb_w: u16, r: Rect, raw: &[u8]) -> Result<(), String> {
    let mut c = Cur { b: raw, i: 0 };
    let mut tile = Vec::with_capacity(64 * 64);
    let mut ty = r.y;
    while ty < r.y + r.h {
        let th = (r.y + r.h - ty).min(64);
        let mut tx = r.x;
        while tx < r.x + r.w {
            let tw = (r.x + r.w - tx).min(64);
            let n = usize::from(tw) * usize::from(th);
            tile.clear();
            let sub = c.u8()?;
            match sub {
                0 => {
                    for _ in 0..n {
                        tile.push(c.cpixel()?);
                    }
                }
                1 => {
                    let p = c.cpixel()?;
                    tile.resize(n, p);
                }
                2..=16 => {
                    let mut pal = Vec::with_capacity(usize::from(sub));
                    for _ in 0..sub {
                        pal.push(c.cpixel()?);
                    }
                    let bits = match sub {
                        2 => 1,
                        3 | 4 => 2,
                        _ => 4,
                    };
                    for _ in 0..th {
                        let mut byte = 0u8;
                        let mut left = 0;
                        for _ in 0..tw {
                            if left == 0 {
                                byte = c.u8()?;
                                left = 8;
                            }
                            left -= bits;
                            let idx = usize::from((byte >> left) & ((1 << bits) - 1));
                            tile.push(*pal.get(idx).ok_or("a ZRLE palette index is out of range")?);
                        }
                    }
                }
                128 => {
                    while tile.len() < n {
                        let p = c.cpixel()?;
                        let run = c.run()?;
                        if tile.len() + run > n {
                            return Err("a ZRLE run goes past the tile".into());
                        }
                        tile.resize(tile.len() + run, p);
                    }
                }
                130..=255 => {
                    let size = usize::from(sub - 128);
                    let mut pal = Vec::with_capacity(size);
                    for _ in 0..size {
                        pal.push(c.cpixel()?);
                    }
                    while tile.len() < n {
                        let i = c.u8()?;
                        let run = if i & 0x80 != 0 { c.run()? } else { 1 };
                        let p = *pal
                            .get(usize::from(i & 0x7f))
                            .ok_or("a ZRLE palette index is out of range")?;
                        if tile.len() + run > n {
                            return Err("a ZRLE run goes past the tile".into());
                        }
                        tile.resize(tile.len() + run, p);
                    }
                }
                s => return Err(format!("unknown ZRLE subencoding {s}")),
            }
            for row in 0..th {
                for col in 0..tw {
                    let p = tile[usize::from(row) * usize::from(tw) + usize::from(col)];
                    let off =
                        ((usize::from(ty + row)) * usize::from(fb_w) + usize::from(tx + col)) * 4;
                    fb[off..off + 4].copy_from_slice(&p);
                }
            }
            tx += tw;
        }
        ty += th;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod fake {
    //! An in-process RFB 3.8 server for the tests: VNC authentication
    //! with a known password, Raw (or ZRLE) updates of a known
    //! framebuffer, and a log of the input it gets.

    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// What the client sent.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Got {
        Pointer { mask: u8, x: u16, y: u16 },
        Key { down: bool, sym: u32 },
    }

    pub struct FakeServer {
        pub address: String,
        pub got: Arc<Mutex<Vec<Got>>>,
        /// Connections that passed authentication.
        pub sessions: Arc<Mutex<u32>>,
        /// A ZRLE update instead of Raw, when the client asks for ZRLE.
        pub encodings: Arc<Mutex<Vec<i32>>>,
    }

    /// The test pixel at x, y: R = x, G = y, B = x ^ y.
    pub fn pixel(x: u16, y: u16) -> [u8; 3] {
        [x as u8, y as u8, (x ^ y) as u8]
    }

    impl FakeServer {
        /// Serve connections on a free localhost port. `password` None:
        /// security type 1. `zrle`: answer with ZRLE when asked.
        pub async fn start(w: u16, h: u16, password: Option<&'static str>, zrle: bool) -> Self {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = l.local_addr().unwrap().to_string();
            let got = Arc::new(Mutex::new(Vec::new()));
            let sessions = Arc::new(Mutex::new(0));
            let encodings = Arc::new(Mutex::new(Vec::new()));
            let (g, s, e) = (got.clone(), sessions.clone(), encodings.clone());
            tokio::spawn(async move {
                while let Ok((sock, _)) = l.accept().await {
                    let (g, s, e) = (g.clone(), s.clone(), e.clone());
                    tokio::spawn(async move {
                        let _ = serve(sock, w, h, password, zrle, g, s, e).await;
                    });
                }
            });
            Self {
                address,
                got,
                sessions,
                encodings,
            }
        }

        pub fn take(&self) -> Vec<Got> {
            std::mem::take(&mut self.got.lock().unwrap())
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve(
        mut s: TcpStream,
        w: u16,
        h: u16,
        password: Option<&'static str>,
        zrle: bool,
        got: Arc<Mutex<Vec<Got>>>,
        sessions: Arc<Mutex<u32>>,
        encodings: Arc<Mutex<Vec<i32>>>,
    ) -> std::io::Result<()> {
        s.write_all(b"RFB 003.008\n").await?;
        let mut v = [0u8; 12];
        s.read_exact(&mut v).await?;
        assert_eq!(&v, b"RFB 003.008\n");
        match password {
            Some(pw) => {
                s.write_all(&[1, 2]).await?;
                assert_eq!(s.read_u8().await?, 2);
                let challenge: [u8; 16] = *b"0123456789abcdef";
                s.write_all(&challenge).await?;
                let mut resp = [0u8; 16];
                s.read_exact(&mut resp).await?;
                if resp != vnc_auth_response(pw.as_bytes(), &challenge) {
                    s.write_u32(1).await?;
                    let why = b"bad password";
                    s.write_u32(why.len() as u32).await?;
                    s.write_all(why).await?;
                    return Ok(());
                }
                s.write_u32(0).await?;
            }
            None => {
                s.write_all(&[1, 1]).await?;
                assert_eq!(s.read_u8().await?, 1);
                s.write_u32(0).await?;
            }
        }
        *sessions.lock().unwrap() += 1;
        assert_eq!(s.read_u8().await?, 1, "shared");
        s.write_u16(w).await?;
        s.write_u16(h).await?;
        // A 16-bit server format, which the client must replace.
        s.write_all(&[16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0])
            .await?;
        s.write_u32(4).await?;
        s.write_all(b"fake").await?;
        let mut bpp = 0u8;
        let mut z = flate2::Compress::new(flate2::Compression::fast(), true);
        loop {
            let t = match s.read_u8().await {
                Ok(t) => t,
                Err(_) => return Ok(()),
            };
            match t {
                0 => {
                    let mut m = [0u8; 19];
                    s.read_exact(&mut m).await?;
                    bpp = m[3];
                    assert_eq!(&m[3..10], &[32, 24, 0, 1, 0, 255, 0]);
                    assert_eq!(&m[13..16], &[16, 8, 0]);
                }
                2 => {
                    s.read_u8().await?;
                    let n = s.read_u16().await?;
                    let mut e = Vec::new();
                    for _ in 0..n {
                        e.push(s.read_i32().await?);
                    }
                    *encodings.lock().unwrap() = e;
                }
                3 => {
                    let mut m = [0u8; 9];
                    s.read_exact(&mut m).await?;
                    assert_eq!(m[0], 0, "a full update");
                    assert_eq!(bpp, 32);
                    let use_zrle = zrle && encodings.lock().unwrap().contains(&ENC_ZRLE);
                    let mut out = vec![0u8, 0];
                    out.extend_from_slice(&1u16.to_be_bytes());
                    for v in [0, 0, w, h] {
                        out.extend_from_slice(&v.to_be_bytes());
                    }
                    if use_zrle {
                        out.extend_from_slice(&ENC_ZRLE.to_be_bytes());
                        let raw = zrle_tiles(w, h);
                        let mut data = Vec::with_capacity(raw.len() * 2 + 1024);
                        z.compress_vec(&raw, &mut data, flate2::FlushCompress::Sync)
                            .unwrap();
                        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
                        out.extend_from_slice(&data);
                    } else {
                        out.extend_from_slice(&ENC_RAW.to_be_bytes());
                        for y in 0..h {
                            for x in 0..w {
                                let [r, g, b] = pixel(x, y);
                                out.extend_from_slice(&[b, g, r, 0]);
                            }
                        }
                    }
                    s.write_all(&out).await?;
                }
                4 => {
                    let mut m = [0u8; 7];
                    s.read_exact(&mut m).await?;
                    let sym = u32::from_be_bytes([m[3], m[4], m[5], m[6]]);
                    got.lock().unwrap().push(Got::Key {
                        down: m[0] == 1,
                        sym,
                    });
                }
                5 => {
                    let mut m = [0u8; 5];
                    s.read_exact(&mut m).await?;
                    got.lock().unwrap().push(Got::Pointer {
                        mask: m[0],
                        x: u16::from_be_bytes([m[1], m[2]]),
                        y: u16::from_be_bytes([m[3], m[4]]),
                    });
                }
                t => panic!("unknown client message {t}"),
            }
        }
    }

    /// ZRLE tiles of the test picture: raw tiles (subencoding 0), with
    /// one solid tile and one plain-RLE tile when the screen has them.
    fn zrle_tiles(w: u16, h: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let mut ty = 0;
        while ty < h {
            let th = (h - ty).min(64);
            let mut tx = 0;
            while tx < w {
                let tw = (w - tx).min(64);
                out.push(0);
                for y in ty..ty + th {
                    for x in tx..tx + tw {
                        let [r, g, b] = pixel(x, y);
                        out.extend_from_slice(&[b, g, r]);
                    }
                }
                tx += tw;
            }
            ty += th;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeServer, Got, pixel};
    use super::*;

    #[test]
    fn des_matches_the_known_vector() {
        // FIPS 81 / the classic DES example: key 133457799BBCDFF1,
        // plain 0123456789ABCDEF, cipher 85E813540F0AB405. The VNC key is
        // the password with the bits of each byte reversed, so the
        // password is the key with its bits reversed.
        let key: [u8; 8] = [0x13, 0x34, 0x57, 0x79, 0x9b, 0xbc, 0xdf, 0xf1];
        let password: Vec<u8> = key.iter().map(|b| b.reverse_bits()).collect();
        assert_eq!(password, [0xc8, 0x2c, 0xea, 0x9e, 0xd9, 0x3d, 0xfb, 0x8f]);
        let mut challenge = [0u8; 16];
        challenge[..8].copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        challenge[8..].copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let r = vnc_auth_response(&password, &challenge);
        let want = [0x85, 0xe8, 0x13, 0x54, 0x0f, 0x0a, 0xb4, 0x05];
        assert_eq!(&r[..8], &want);
        assert_eq!(&r[8..], &want);
        // Only the first 8 bytes of a password count; a short one is
        // padded with zeros.
        let c = *b"0123456789abcdef";
        assert_eq!(
            vnc_auth_response(b"secret-password", &c),
            vnc_auth_response(b"secret-p", &c)
        );
        assert_ne!(
            vnc_auth_response(b"secret", &c),
            vnc_auth_response(b"secret\0x", &c)
        );
    }

    #[test]
    fn text_keysyms() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('A'), 0x41);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('€'), 0x0100_20ac);
        assert_eq!(char_keysym('\n'), 0xff0d);
        assert_eq!(char_keysym('\t'), 0xff09);
    }

    #[test]
    fn zrle_subencodings_decode() {
        // One 4x2 tile for each subencoding, all into a 4x2 screen.
        let r = Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 2,
        };
        let red = [0, 0, 255];
        let blue = [255, 0, 0];
        let px = |c: [u8; 3]| [c[0], c[1], c[2], 0];
        let decode = |raw: Vec<u8>| {
            let mut fb = vec![9u8; 4 * 2 * 4];
            zrle(&mut fb, 4, r, &raw).unwrap();
            fb.chunks_exact(4)
                .map(|p| [p[0], p[1], p[2], p[3]])
                .collect::<Vec<_>>()
        };
        // Solid.
        let mut raw = vec![1];
        raw.extend_from_slice(&red);
        assert_eq!(decode(raw), vec![px(red); 8]);
        // Packed palette of 2: 1 bit each, rows padded to a byte.
        let mut raw = vec![2];
        raw.extend_from_slice(&red);
        raw.extend_from_slice(&blue);
        raw.extend_from_slice(&[0b0101_0000, 0b1000_0000]);
        assert_eq!(
            decode(raw),
            vec![
                px(red),
                px(blue),
                px(red),
                px(blue),
                px(blue),
                px(red),
                px(red),
                px(red)
            ]
        );
        // Plain RLE: 3 red, 5 blue.
        let mut raw = vec![128];
        raw.extend_from_slice(&red);
        raw.push(2);
        raw.extend_from_slice(&blue);
        raw.push(4);
        let fb = decode(raw);
        assert_eq!(&fb[..3], &[px(red); 3]);
        assert_eq!(&fb[3..], &[px(blue); 5]);
        // Palette RLE of 2: index 1 alone, then index 0 with a run of 7.
        let mut raw = vec![130];
        raw.extend_from_slice(&red);
        raw.extend_from_slice(&blue);
        raw.extend_from_slice(&[1, 0x80, 6]);
        let fb = decode(raw);
        assert_eq!(fb[0], px(blue));
        assert_eq!(&fb[1..], &[px(red); 7]);
        // A run past the tile is an error, not a panic.
        let mut raw = vec![128];
        raw.extend_from_slice(&red);
        raw.push(20);
        let mut fb = vec![0u8; 32];
        assert!(zrle(&mut fb, 4, r, &raw).is_err());
        assert!(zrle(&mut fb, 4, r, &[0, 1, 2]).is_err());
    }

    fn check_pixels(png: &[u8], w: u16, h: u16) {
        let img = Image::decode(png).unwrap();
        assert_eq!((img.width, img.height), (u32::from(w), u32::from(h)));
        for (x, y) in [(0, 0), (w - 1, 0), (5, 7), (w - 1, h - 1), (w / 2, h / 3)] {
            let i = (usize::from(y) * usize::from(w) + usize::from(x)) * 4;
            let [r, g, b] = pixel(x, y);
            assert_eq!(&img.rgba[i..i + 4], &[r, g, b, 255], "pixel {x},{y}");
        }
    }

    #[tokio::test]
    async fn auth_raw_and_zrle_screenshots() {
        let t = Duration::from_secs(5);
        for zrle in [false, true] {
            let s = FakeServer::start(130, 70, Some("s3cret"), zrle).await;
            let mut c = VncConn::connect(&s.address, Some("s3cret"), t)
                .await
                .unwrap();
            assert_eq!(
                (c.width, c.height, c.version, c.security),
                (130, 70, "3.8", "vnc")
            );
            assert_eq!(c.name, "fake");
            let png = c.screenshot(t).await.unwrap();
            check_pixels(&png, 130, 70);
            // A second update on the same connection, and the zlib stream
            // carries on.
            let png = c.screenshot(t).await.unwrap();
            check_pixels(&png, 130, 70);
            assert_eq!(
                s.encodings.lock().unwrap()[..3],
                [ENC_ZRLE, ENC_COPY_RECT, ENC_RAW]
            );
            c.drain(t).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_wrong_or_missing_password_fails_without_the_password() {
        let t = Duration::from_secs(5);
        let s = FakeServer::start(8, 8, Some("right"), false).await;
        let e = VncConn::connect(&s.address, Some("wrong-pw-xyz"), t)
            .await
            .err()
            .unwrap();
        let msg = e.to_string();
        assert!(msg.contains("authentication failed"), "{msg}");
        assert!(!msg.contains("wrong-pw-xyz"), "{msg}");
        let e = VncConn::connect(&s.address, None, t).await.err().unwrap();
        assert!(e.to_string().contains("asks for a password"), "{e}");
        assert_eq!(*s.sessions.lock().unwrap(), 0);
        // No password needed.
        let s = FakeServer::start(8, 8, None, false).await;
        let c = VncConn::connect(&s.address, Some("ignored"), t)
            .await
            .unwrap();
        assert_eq!(c.security, "none");
    }

    #[tokio::test]
    async fn input_events_reach_the_server_in_order() {
        let t = Duration::from_secs(5);
        let s = FakeServer::start(100, 50, None, false).await;
        let mut c = VncConn::connect(&s.address, None, t).await.unwrap();
        c.input(&[
            Ev::Move(500, -3),
            Ev::Mask(1),
            Ev::Pause(1),
            Ev::Mask(0),
            Ev::Key(0xffe3, true),
            Ev::Key(0x6c, true),
            Ev::Key(0x6c, false),
            Ev::Key(0xffe3, false),
        ])
        .await
        .unwrap();
        // A screenshot is a round trip: the events are in by then.
        c.screenshot(t).await.unwrap();
        assert_eq!(
            s.take(),
            [
                Got::Pointer {
                    mask: 0,
                    x: 99,
                    y: 0
                },
                Got::Pointer {
                    mask: 1,
                    x: 99,
                    y: 0
                },
                Got::Pointer {
                    mask: 0,
                    x: 99,
                    y: 0
                },
                Got::Key {
                    down: true,
                    sym: 0xffe3
                },
                Got::Key {
                    down: true,
                    sym: 0x6c
                },
                Got::Key {
                    down: false,
                    sym: 0x6c
                },
                Got::Key {
                    down: false,
                    sym: 0xffe3
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_closed_server_shows_in_drain() {
        let t = Duration::from_secs(5);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.write_all(b"RFB 003.003\n").await.unwrap();
            let mut v = [0u8; 12];
            s.read_exact(&mut v).await.unwrap();
            assert_eq!(&v, b"RFB 003.003\n");
            s.write_u32(1).await.unwrap();
            assert_eq!(s.read_u8().await.unwrap(), 1);
            s.write_all(&[0, 4, 0, 4]).await.unwrap();
            s.write_all(&[0u8; 16]).await.unwrap();
            s.write_u32(0).await.unwrap();
            // Bell, then close.
            let mut rest = vec![0u8; 20 + 4 + 4 * 4];
            s.read_exact(&mut rest).await.unwrap();
            s.write_u8(2).await.unwrap();
        });
        let mut c = VncConn::connect(&addr, None, t).await.unwrap();
        assert_eq!(c.version, "3.3");
        server.await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let e = c.drain(t).await.unwrap_err();
        assert!(e.to_string().contains("closed"), "{e}");
    }
}
