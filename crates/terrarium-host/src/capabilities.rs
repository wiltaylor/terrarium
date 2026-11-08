//! Tool detection. The host agent runs it on start and sends the result in
//! `hello`. `terrarium-host check` prints it.

use std::path::Path;

use crate::config::DisplayConfig;
use crate::protocol::HostCapabilities;

/// Detect Docker (`docker version --format json`), vmlab
/// (`vmlab --version`), the login shell, the metrics sources (`/proc` on
/// Linux), and resource limits (`systemd-run --user`). The desktop display
/// is off.
pub async fn detect(docker_bin: &str, vmlab_bin: &str) -> HostCapabilities {
    detect_with(docker_bin, vmlab_bin, &DisplayConfig::default()).await
}

/// [`detect`], with the display part from the `[display]` config: the
/// opt-in, `tesseract`, and the desktop tools.
pub async fn detect_with(
    docker_bin: &str,
    vmlab_bin: &str,
    display: &DisplayConfig,
) -> HostCapabilities {
    let (docker, vmlab, mut limits) = tokio::join!(
        crate::docker::detect(docker_bin),
        crate::vmlab::detect(vmlab_bin),
        crate::limits::detect()
    );
    let display = crate::display::detect(display, docker.present, vmlab.present);
    limits.docker = docker.present;
    HostCapabilities {
        capacity: detect_capacity(Path::new("/proc")),
        docker,
        vmlab,
        shell: detect_shell(),
        display,
        metrics: crate::metrics::Sampler::available(),
        limits: Some(limits),
        // The caller sets it from its work_dir (`crate::workspace::available`).
        no_workspaces: false,
    }
}

/// Read host logical CPUs and physical memory, not the process affinity or
/// current free memory. A missing source means unknown capacity.
pub fn detect_capacity(proc_root: &Path) -> Option<terrarium_core::resources::HostCapacity> {
    let stat = std::fs::read_to_string(proc_root.join("stat")).ok()?;
    let mut cpu_ids: Vec<u32> = stat
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .next()?
                .strip_prefix("cpu")?
                .parse()
                .ok()
        })
        .collect();
    cpu_ids.sort_unstable();
    cpu_ids.dedup();
    if cpu_ids.is_empty() {
        return None;
    }
    let mem = std::fs::read_to_string(proc_root.join("meminfo")).ok()?;
    let memory_bytes = mem.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next()? != "MemTotal:" {
            return None;
        }
        let kb = words.next()?.parse::<u64>().ok()?;
        if words.next()? != "kB" {
            return None;
        }
        kb.checked_mul(1024).filter(|n| *n > 0)
    })?;
    Some(terrarium_core::resources::HostCapacity {
        cpu_ids,
        memory_bytes,
    })
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
        // Docker limits need Docker.
        assert!(!caps.limits.expect("limits capability").docker);
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
