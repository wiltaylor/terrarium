//! Native GPU isolation uses Linux device policies and is unavailable on Windows.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_SOCKET: &str = "";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeOptions {
    pub socket: PathBuf,
    pub shim: PathBuf,
}

/// Wire-compatible provisioning request. Windows never executes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedRoot {
    #[serde(default)]
    pub cleanup: bool,
    pub base: String,
    pub project: String,
    pub name: String,
    pub agent: String,
}

pub async fn capability(_: &Path) -> terrarium_core::protocol::NativeGpuCapability {
    terrarium_core::protocol::NativeGpuCapability {
        enforced: false,
        version: 0,
        reason: Some("native GPU isolation is unavailable on Windows".into()),
    }
}

pub async fn shim(_: &Path, _: bool) -> i32 {
    eprintln!("native GPU isolation is unavailable on Windows");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_gpu_is_always_refused() {
        let cap = capability(Path::new("ignored")).await;
        assert!(!cap.enforced);
        assert!(cap.reason.unwrap().contains("unavailable on Windows"));
        assert_eq!(shim(Path::new("ignored"), false).await, 1);
    }
}
