//! Bounded private pipe protocol for the Windows per-session runner.
//! The runner joins its Job Object before creating any command process.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::{ExitReason, PtySize, StreamTag};
use crate::session::ExecPlan;

const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    pty: Option<PtySize>,
}

impl From<&ExecPlan> for Plan {
    fn from(p: &ExecPlan) -> Self {
        Self {
            program: p.program.clone(), args: p.args.clone(), cwd: p.cwd.clone(),
            env: p.env.clone(), pty: p.pty,
        }
    }
}

impl From<Plan> for ExecPlan {
    fn from(p: Plan) -> Self {
        Self {
            program: p.program, args: p.args, cwd: p.cwd, env: p.env,
            pty: p.pty, ..Default::default()
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Input {
    Stdin(Vec<u8>),
    Eof,
    Resize(PtySize),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Output {
    Data(StreamTag, Vec<u8>),
    Exit { code: Option<i32>, signal: Option<i32>, reason: ExitReason },
    Error(String),
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, value: &T) -> std::io::Result<()> {
    let data = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    if data.len() > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "runner frame too large"));
    }
    w.write_u32(data.len() as u32).await?;
    w.write_all(&data).await?;
    w.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(r: &mut R) -> std::io::Result<T> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "runner frame too large"));
    }
    let mut data = vec![0; len];
    r.read_exact(&mut data).await?;
    serde_json::from_slice(&data).map_err(std::io::Error::other)
}

#[cfg(windows)]
pub mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn protocol_roundtrips_binary_stdin_and_rejects_oversize_frames() {
        let (mut a, mut b) = tokio::io::duplex(MAX_FRAME * 2);
        write_frame(&mut a, &Input::Stdin(vec![0, 255, 13, 10])).await.unwrap();
        let Input::Stdin(data) = read_frame(&mut b).await.unwrap() else { panic!("stdin") };
        assert_eq!(data, [0, 255, 13, 10]);
        a.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
        assert_eq!(read_frame::<_, Input>(&mut b).await.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn protocol_preserves_plan_arguments_without_shell_interpolation() {
        let p = ExecPlan {
            program: "example.exe".into(), args: vec!["a b".into(), "$(bad);&".into()],
            ..Default::default()
        };
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_frame(&mut a, &Plan::from(&p)).await.unwrap();
        let actual: Plan = read_frame(&mut b).await.unwrap();
        assert_eq!(ExecPlan::from(actual), p);
    }
}
