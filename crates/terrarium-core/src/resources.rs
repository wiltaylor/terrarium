//! Host reservation accounting. Quantities are integers to make admission
//! deterministic. IO capacities are configured budgets, not sampled traffic.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
            let canonical = major.parse::<u32>().ok().zip(minor.parse::<u32>().ok())
                .map(|(a, b)| format!("{a}:{b}"));
            if canonical.as_deref() != Some(device.as_str()) {
                return Err(format!("invalid IO device {device}: use canonical major:minor"));
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
            io: self.io.iter().map(|(device, io)| (device.clone(), IoAmounts {
                read_bps: percentage(io.read_bps, percent),
                write_bps: percentage(io.write_bps, percent),
                read_iops: percentage(io.read_iops, percent),
                write_iops: percentage(io.write_iops, percent),
            })).collect(),
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
        self.cpu_millis == 0 && self.memory_bytes == 0 && self.io.values().all(|v|
            v.read_bps == 0 && v.write_bps == 0 && v.read_iops == 0 && v.write_iops == 0)
    }
}

/// A host reports logical CPU IDs and physical memory. Device throughput
/// cannot be inferred from an instantaneous sample and is set by an admin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapacity {
    pub cpu_ids: Vec<u32>,
    pub memory_bytes: u64,
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
