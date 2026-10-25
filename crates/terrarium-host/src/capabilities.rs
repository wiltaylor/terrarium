//! Tool detection. The host agent runs it on start and sends the result in
//! `hello`. `terrarium-host check` prints it.

use std::path::Path;

use crate::protocol::HostCapabilities;

/// Detect Docker (`docker version --format json`), vmlab
/// (`vmlab --version`), and the login shell.
pub async fn detect(docker_bin: &str, vmlab_bin: &str) -> HostCapabilities {
    let (docker, vmlab) = tokio::join!(
        crate::docker::detect(docker_bin),
        crate::vmlab::detect(vmlab_bin)
    );
    HostCapabilities {
        docker,
        vmlab,
        shell: detect_shell(),
        can_update_harnesses: false,
    }
}

/// `$SHELL` when it exists, else `/bin/sh`.
pub fn detect_shell() -> Option<String> {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty() && Path::new(s).exists())
        .or_else(|| Path::new("/bin/sh").exists().then(|| "/bin/sh".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_tools_are_absent() {
        let caps = detect("/nonexistent/docker", "/nonexistent/vmlab").await;
        assert!(!caps.docker.present);
        assert!(!caps.vmlab.present);
        assert!(caps.shell.is_some());
    }

    /// Calls the real `docker` and `vmlab`. Run with `--ignored`.
    #[tokio::test]
    #[ignore = "calls the real docker and vmlab"]
    async fn real_tools() {
        let caps = detect("docker", "vmlab").await;
        eprintln!("{caps:#?}");
        assert!(caps.docker.present, "docker: {:?}", caps.docker);
        assert!(caps.vmlab.present, "vmlab: {:?}", caps.vmlab);
    }
}
