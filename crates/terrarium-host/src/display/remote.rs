//! The VNC (RFB) and RDP drivers. Step 1 has stubs only: each operation
//! answers `unsupported`, "not implemented yet (step 2)". The daemon side,
//! the frame, the address checks, and the secret path are in place.
//!
//! # Step 2
//!
//! Replace the bodies of the [`DisplayDriver`] impls below:
//!
//! - **VNC**: connect to `address` over TCP, do the RFB handshake (3.8;
//!   security type 2, VNC authentication, with `password`; type 1 when the
//!   server needs none), ask for a full framebuffer update, and make the
//!   PNG with [`Image::from_bgrx`](super::image::Image::from_bgrx) (or
//!   `from_rgba`) and `encode`. Pointer: `PointerEvent` with the button
//!   mask (bit 0 left, 1 middle, 2 right, 3 and 4 wheel up and down, 5
//!   and 6 wheel left and right). Keys: `KeyEvent` with
//!   [`Key::x_keysym_value`](terrarium_core::display::Key::x_keysym_value)
//!   and [`Modifier::x_keysym_value`](terrarium_core::display::Modifier::x_keysym_value);
//!   text: one keysym for each character (Latin-1 is the code point,
//!   other Unicode is `0x0100_0000 + code point`). Set
//!   [`Features::scroll`] to true.
//! - **RDP**: IronRDP (`ironrdp-connector`, `ironrdp-session`, with
//!   `ironrdp-tls` on rustls/ring, no OpenSSL, so the static musl build
//!   stays). Log in with `username`, `domain`, and `password` (NLA/CredSSP),
//!   take the bitmap updates into a framebuffer, and send fast-path input:
//!   mouse events, scancodes for chords, and Unicode key events for text.
//!
//! Each operation may open its own connection (simple), or the driver may
//! keep one connection for each target in the host agent for a short time
//! (faster: a live view takes a screenshot every 2 to 3 s). The password
//! comes only in the `display` frame; never log it ([`DisplayTarget`]'s
//! `Debug` hides it).
//!
//! [`DisplayTarget`]: crate::protocol::DisplayTarget

use terrarium_core::display::{Button, KeyChord};

use super::{DResult, DisplayDriver, DisplayError};

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

fn step2(proto: &str) -> DisplayError {
    DisplayError::Unsupported(format!(
        "the {proto} display driver is not implemented yet (step 2); the host agent can \
         reach {proto} targets once it is"
    ))
}

/// A VNC server. Step 1: a stub.
pub struct VncDriver {
    pub address: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl VncDriver {
    pub fn new(addr: &str, username: Option<String>, password: Option<String>) -> DResult<Self> {
        Ok(Self {
            address: address(addr, VNC_PORT)?,
            username,
            password,
        })
    }
}

/// An RDP server. Step 1: a stub.
pub struct RdpDriver {
    pub address: String,
    pub username: Option<String>,
    pub domain: Option<String>,
    pub password: Option<String>,
}

impl RdpDriver {
    pub fn new(
        addr: &str,
        username: Option<String>,
        domain: Option<String>,
        password: Option<String>,
    ) -> DResult<Self> {
        Ok(Self {
            address: address(addr, RDP_PORT)?,
            username,
            domain,
            password,
        })
    }
}

macro_rules! stub_driver {
    ($t:ty, $name:literal) => {
        impl DisplayDriver for $t {
            fn backend(&self) -> &'static str {
                $name
            }
            async fn screenshot(&self) -> DResult<Vec<u8>> {
                Err(step2($name))
            }
            async fn move_to(&self, _x: i32, _y: i32) -> DResult<()> {
                Err(step2($name))
            }
            async fn click(
                &self,
                _at: Option<(i32, i32)>,
                _button: Button,
                _double: bool,
            ) -> DResult<()> {
                Err(step2($name))
            }
            async fn drag(&self, _from: (i32, i32), _to: (i32, i32)) -> DResult<()> {
                Err(step2($name))
            }
            async fn scroll(&self, _at: Option<(i32, i32)>, _dx: i32, _dy: i32) -> DResult<()> {
                Err(step2($name))
            }
            async fn keys(&self, _chord: &KeyChord) -> DResult<()> {
                Err(step2($name))
            }
            async fn type_text(&self, _text: &str) -> DResult<()> {
                Err(step2($name))
            }
        }
    };
}

stub_driver!(VncDriver, "vnc");
stub_driver!(RdpDriver, "rdp");

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

    #[tokio::test]
    async fn stubs_say_step_2_for_every_op() {
        let fake = FakeRunner::ok();
        let c = ctx(fake.clone());
        for target in [
            DisplayTarget::Vnc {
                address: "10.0.0.5".into(),
                username: None,
                password: Some("pw".into()),
            },
            DisplayTarget::Rdp {
                address: "win".into(),
                username: Some("bob".into()),
                domain: Some("LAB".into()),
                password: Some("pw".into()),
            },
        ] {
            for op in [
                DisplayOp::Info,
                DisplayOp::Move { x: 1, y: 1 },
                DisplayOp::Keys { chord: "a".into() },
                DisplayOp::Type { text: "a".into() },
            ] {
                let e = dispatch(&c, &target, &op).await.unwrap_err();
                assert_eq!(e.code(), ErrorCode::Unsupported);
                assert!(
                    e.to_string().contains("not implemented yet (step 2)"),
                    "{e}"
                );
            }
        }
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
