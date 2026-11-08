//! Host reservation accounting. Quantities are integers to make admission
//! deterministic. IO capacities are configured budgets, not sampled traffic.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Fully resolved enforcement values. `cpus: None` clears affinity, while
/// `Some` must contain at least one logical CPU. Device keys are major:minor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLimits {
    /// Create-time Docker GPU allowlist. Empty means no GPU access.
    #[serde(default)]
    pub gpu_devices: Vec<String>,
    pub limits: crate::limits::Limits,
    #[serde(default)]
    pub cpus: Option<Vec<u32>>,
    #[serde(default)]
    pub io: BTreeMap<String, IoAmounts>,
}

impl RuntimeLimits {
    /// Docker treats zero CPU/memory and empty cpuset updates as no change.
    /// Resolve absent dimensions to explicit managed host defaults instead.
    pub fn docker_defaults(&mut self, capacity: &HostCapacity) -> Result<(), String> {
        if capacity.cpu_ids.is_empty() || capacity.memory_bytes == 0 {
            return Err("host capacity is incomplete".into());
        }
        let cpu = u32::try_from(capacity.cpu_ids.len())
            .ok()
            .and_then(|n| n.checked_mul(1000))
            .ok_or("host CPU capacity overflow")?;
        self.limits.cpu_millis.get_or_insert(cpu);
        self.limits
            .memory_bytes
            .get_or_insert(capacity.memory_bytes);
        self.limits.io_weight.get_or_insert(100);
        self.cpus.get_or_insert_with(|| capacity.cpu_ids.clone());
        self.validate()
    }

    pub fn validate(&self) -> Result<(), String> {
        for id in &self.gpu_devices {
            validate_gpu_id(id)?;
        }
        Amounts {
            io: self.io.clone(),
            ..Default::default()
        }
        .validate()?;
        if self.cpus.as_ref().is_some_and(Vec::is_empty) {
            return Err("CPU affinity cannot be empty".into());
        }
        Ok(())
    }

    pub fn cpuset(&self) -> String {
        self.cpus
            .as_ref()
            .map(|v| v.iter().map(u32::to_string).collect::<Vec<_>>().join(","))
            .unwrap_or_default()
    }

    pub fn host_support(
        &self,
        cap: Option<&crate::limits::LimitsCapability>,
    ) -> Result<(), String> {
        self.validate()?;
        self.limits.host_support(cap)?;
        let controllers = &cap.ok_or("missing resource limits capability")?.controllers;
        if self.cpus.is_some() && !controllers.iter().any(|c| c == "cpuset") {
            return Err("missing delegated cpuset controller".into());
        }
        if !self.io.is_empty() && !controllers.iter().any(|c| c == "io") {
            return Err("missing delegated io controller".into());
        }
        Ok(())
    }
}

/// Per-device throughput and operation budgets. Zero means no allocation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IoAmounts {
    #[serde(default)]
    pub read_bps: u64,
    #[serde(default)]
    pub write_bps: u64,
    #[serde(default)]
    pub read_iops: u64,
    #[serde(default)]
    pub write_iops: u64,
}

/// CPU in thousandths of a logical core and memory in bytes. IO keys are
/// host block device numbers (`major:minor`), not arbitrary filesystem paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Amounts {
    #[serde(default)]
    pub cpu_millis: u64,
    #[serde(default)]
    pub memory_bytes: u64,
    #[serde(default)]
    pub io: BTreeMap<String, IoAmounts>,
}

fn percentage(n: u64, percent: u8) -> u64 {
    // Avoid overflow even for a capacity of u64::MAX. Round down, never up.
    n / 100 * u64::from(percent) + n % 100 * u64::from(percent) / 100
}

impl Amounts {
    pub fn validate(&self) -> Result<(), String> {
        for device in self.io.keys() {
            let Some((major, minor)) = device.split_once(':') else {
                return Err(format!("invalid IO device {device}: use major:minor"));
            };
            let canonical = major
                .parse::<u32>()
                .ok()
                .zip(minor.parse::<u32>().ok())
                .map(|(a, b)| format!("{a}:{b}"));
            if canonical.as_deref() != Some(device.as_str()) {
                return Err(format!(
                    "invalid IO device {device}: use canonical major:minor"
                ));
            }
        }
        Ok(())
    }

    /// The portion the operator allows agents to reserve. Zero disables
    /// reservations on the host. Percentages above 100 are never accepted.
    pub fn reservable(&self, percent: u8) -> Result<Self, String> {
        if percent > 100 {
            return Err("reservable percentage must be between 0 and 100".into());
        }
        self.validate()?;
        Ok(Self {
            cpu_millis: percentage(self.cpu_millis, percent),
            memory_bytes: percentage(self.memory_bytes, percent),
            io: self
                .io
                .iter()
                .map(|(device, io)| {
                    (
                        device.clone(),
                        IoAmounts {
                            read_bps: percentage(io.read_bps, percent),
                            write_bps: percentage(io.write_bps, percent),
                            read_iops: percentage(io.read_iops, percent),
                            write_iops: percentage(io.write_iops, percent),
                        },
                    )
                })
                .collect(),
        })
    }

    /// Subtract a reservation, failing closed on unknown devices or any
    /// exhausted dimension. The original budget is never partially changed.
    pub fn remaining_after(&self, request: &Self) -> Option<Self> {
        let mut remaining = self.clone();
        remaining.cpu_millis = self.cpu_millis.checked_sub(request.cpu_millis)?;
        remaining.memory_bytes = self.memory_bytes.checked_sub(request.memory_bytes)?;
        for (device, amount) in &request.io {
            let available = remaining.io.get_mut(device)?;
            available.read_bps = available.read_bps.checked_sub(amount.read_bps)?;
            available.write_bps = available.write_bps.checked_sub(amount.write_bps)?;
            available.read_iops = available.read_iops.checked_sub(amount.read_iops)?;
            available.write_iops = available.write_iops.checked_sub(amount.write_iops)?;
        }
        Some(remaining)
    }

    pub fn is_empty(&self) -> bool {
        self.cpu_millis == 0
            && self.memory_bytes == 0
            && self.io.values().all(|v| {
                v.read_bps == 0 && v.write_bps == 0 && v.read_iops == 0 && v.write_iops == 0
            })
    }
}

/// A host reports logical CPU IDs and physical memory. Device throughput
/// cannot be inferred from an instantaneous sample and is set by an admin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapacity {
    pub cpu_ids: Vec<u32>,
    pub memory_bytes: u64,
}

/// Stable NVIDIA UUID, rather than an index that can change after reboot.
pub fn validate_gpu_id(id: &str) -> Result<(), String> {
    if !id.starts_with("GPU-")
        || id.len() <= 4
        || !id[4..].bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return Err("GPU device must be an NVIDIA GPU UUID (GPU-...)".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub uuid: String,
    pub name: String,
    pub memory_bytes: u64,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuClaim {
    pub uuid: String,
    /// Cooperative share for shared access. Exclusive access reserves the device.
    #[serde(default)]
    pub vram_bytes: u64,
    #[serde(default)]
    pub exclusive: bool,
}

impl HostCapacity {
    pub fn amounts(&self) -> Amounts {
        let mut ids = self.cpu_ids.clone();
        ids.sort_unstable();
        ids.dedup();
        Amounts {
            cpu_millis: (ids.len() as u64).saturating_mul(1000),
            memory_bytes: self.memory_bytes,
            ..Default::default()
        }
    }
}
