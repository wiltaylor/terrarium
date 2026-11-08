//! Bounded NVIDIA inventory/VRAM sampling. No MIG allocation or configuration.
use terrarium_core::resources::{GpuDevice, validate_gpu_id};

pub fn parse(text: &str) -> Option<Vec<GpuDevice>> {
    let mut devices = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        if fields.len() != 4 { return None; }
        validate_gpu_id(fields[0]).ok()?;
        let memory_bytes = fields[2].parse::<u64>().ok()?.checked_mul(1 << 20)?;
        let used_bytes = fields[3].parse::<u64>().ok()?.checked_mul(1 << 20)?;
        if memory_bytes == 0 || used_bytes > memory_bytes || devices.iter().any(|d: &GpuDevice| d.uuid == fields[0]) { return None; }
        devices.push(GpuDevice { uuid: fields[0].into(), name: fields[1].into(), memory_bytes, used_bytes });
    }
    Some(devices)
}

pub async fn sample() -> Option<Vec<GpuDevice>> {
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=uuid,name,memory.total,memory.used", "--format=csv,noheader,nounits"]).kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(2), cmd.output()).await.ok()?.ok()?;
    if !output.status.success() { return None; }
    parse(std::str::from_utf8(&output.stdout).ok()?)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub uuid: String,
    pub container: String,
    pub used_bytes: u64,
}

/// Attribute compute VRAM using host PID cgroups, never whole-device totals.
pub async fn usage(docker: &str) -> Vec<Usage> {
    async fn output(bin: &str, args: &[&str]) -> Option<String> {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args).kill_on_drop(true);
        let out = tokio::time::timeout(std::time::Duration::from_secs(2), cmd.output()).await.ok()?.ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
    let Some(processes) = output("nvidia-smi", &["--query-compute-apps=gpu_uuid,pid,used_gpu_memory", "--format=csv,noheader,nounits"]).await else { return vec![]; };
    if processes.trim().is_empty() { return vec![]; }
    let Some(containers) = output(docker, &["ps", "--no-trunc", "--format", "{{.ID}} {{.Names}}"]).await else { return vec![]; };
    attribute(&processes, &containers, |pid| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok())
}

pub fn attribute(processes: &str, containers: &str, cgroup: impl Fn(u32) -> Option<String>) -> Vec<Usage> {
    let mut totals = std::collections::BTreeMap::<(String, String), u64>::new();
    for line in processes.lines() {
        let f: Vec<_> = line.split(',').map(str::trim).collect();
        if f.len() != 3 || validate_gpu_id(f[0]).is_err() { continue; }
        let (Ok(pid), Ok(mib)) = (f[1].parse::<u32>(), f[2].parse::<u64>()) else { continue; };
        let Some(bytes) = mib.checked_mul(1 << 20) else { continue; };
        let Some(group) = cgroup(pid) else { continue; };
        for container in containers.lines() {
            let Some((id, name)) = container.split_once(' ') else { continue; };
            if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) && group.split(|c: char| !c.is_ascii_hexdigit()).any(|part| part == id) {
                let total = totals.entry((f[0].into(), name.into())).or_default();
                *total = total.saturating_add(bytes);
                break;
            }
        }
    }
    totals.into_iter().map(|((uuid, container), used_bytes)| Usage { uuid, container, used_bytes }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compute_usage_is_attributed_to_container_not_device_total() {
        let id = "a".repeat(64);
        let containers = format!("{id} terrarium-box");
        let result = attribute("GPU-abcd, 12, 128\nGPU-abcd, 13, 64\nGPU-abcd, 14, 4096\nGPU-abcd, 15, N/A", &containers, |pid| {
            Some(if pid == 12 || pid == 13 { format!("0::/system.slice/docker-{id}.scope") } else { "0::/user.slice/native.scope".into() })
        });
        assert_eq!(result, vec![Usage { uuid: "GPU-abcd".into(), container: "terrarium-box".into(), used_bytes: 192 << 20 }]);
    }
    #[test]
    fn inventory_and_memory_are_strict() {
        let devices = parse("GPU-abcd, NVIDIA RTX 5090, 32768, 1024\nGPU-1234, NVIDIA RTX 4060, 8192, 0\n").unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].memory_bytes, 32 << 30);
        assert_eq!(devices[0].used_bytes, 1 << 30);
        for invalid in ["GPU-abcd, test, N/A, 0", "MIG-abcd, test, 1, 0", "GPU-abcd, test, 1, 2", "GPU-abcd, test, 18446744073709551615, 0", "GPU-abcd, test, 1, 0\nGPU-abcd, test, 1, 0"] {
            assert!(parse(invalid).is_none());
        }
        assert_eq!(parse(""), Some(vec![]));
    }
}
