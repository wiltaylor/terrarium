//! Linux-only native GPU helper. Windows never exposes this authority.

#[cfg(unix)]
#[path = "../helper_unix.rs"]
mod platform;

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    platform::run()
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("native GPU enforcement is unsupported on Windows")
}
