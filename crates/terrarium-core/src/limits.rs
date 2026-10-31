//! Resource limits of a target (docs/api.md §17.2, "Resource limits").
//!
//! A target definition holds [`TargetLimits`]: the values as the operator
//! wrote them (`cpu = 2`, `memory = "4G"`). [`TargetLimits::resolve`] checks
//! them and gives [`Limits`]: plain numbers, which the daemon sends to the
//! host agent. The host agent applies them with `systemd-run --user` on a
//! `host` target, and with `docker run` flags on a `docker` target.
//!
//! The daemon sends limits only to a host agent that has the
//! [`LimitsCapability`] for them. [`Limits::host_support`] and
//! [`Limits::docker_support`] make that check, so a session never runs
//! without its limits.

use serde::{Deserialize, Serialize};

/// The most CPU cores that `cpu` may give.
pub const MAX_CPU: f64 = 1024.0;
/// The smallest `memory`. Docker refuses less than 6 MiB, and a shell
/// needs more.
pub const MIN_MEMORY: u64 = 16 * 1024 * 1024;
/// The most tasks that `pids` may give (the kernel's `pid_max` limit).
pub const MAX_PIDS: u64 = 4_194_304;

/// A limit value as the operator wrote it: a number, or a text such as
/// `"4G"` or `"0.5"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LimitValue {
    Int(i64),
    Float(f64),
    Text(String),
}

// A limit value is never NaN: `resolve` refuses it.
impl Eq for LimitValue {}

impl LimitValue {
    /// The value as a number. A text must hold a plain number.
    fn number(&self) -> Option<f64> {
        match self {
            #[allow(clippy::cast_precision_loss)]
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            Self::Text(t) => t.trim().parse().ok(),
        }
        .filter(|f: &f64| f.is_finite())
    }

    /// The value as a whole number.
    fn whole(&self) -> Option<u64> {
        match self {
            Self::Int(i) => u64::try_from(*i).ok(),
            Self::Float(_) => None,
            Self::Text(t) => t.trim().parse().ok(),
        }
    }

    fn show(&self) -> String {
        match self {
            Self::Int(i) => i.to_string(),
            Self::Float(f) => f.to_string(),
            Self::Text(t) => format!("\"{t}\""),
        }
    }
}

/// The `limits` of a target definition (`[limits]` in a target file).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetLimits {
    /// CPU time as cores: `2` is two cores, `0.5` is half of one core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<LimitValue>,
    /// The memory cap: bytes, or a size with `K`, `M`, `G`, or `T`
    /// (powers of 1024), for example `"4G"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<LimitValue>,
    /// The swap that may come on top of `memory`. Default: no swap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<LimitValue>,
    /// The most processes and threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<LimitValue>,
    /// The IO weight, 1 to 10000 (default of the kernel: 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_weight: Option<LimitValue>,
}

/// Checked limits in plain numbers. The daemon sends them to the host
/// agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// CPU time in thousandths of one core: `2000` is two cores. Always a
    /// multiple of 10 (a whole percent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// The swap on top of `memory_bytes`. `None` with `memory_bytes`: no
    /// swap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_weight: Option<u16>,
}

/// What a host agent can do for [`Limits`] (docs/host-protocol.md,
/// "Resource limits"). An older host agent sends none: it cannot apply
/// limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsCapability {
    /// `systemd-run --user --scope` works with a `MemoryMax`: a `host`
    /// target can have limits.
    #[serde(default)]
    pub systemd: bool,
    /// The cgroup controllers that the systemd user manager can give to
    /// its units (its `cgroup.controllers`), for example `cpu`, `io`,
    /// `memory`, `pids`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controllers: Vec<String>,
    /// Why `systemd` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `docker.run` takes `limits`. True when Docker is present.
    #[serde(default)]
    pub docker: bool,
}

impl TargetLimits {
    /// No limit is set.
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none()
            && self.memory.is_none()
            && self.swap.is_none()
            && self.pids.is_none()
            && self.io_weight.is_none()
    }

    /// Check the values and give them as numbers. The error is a sentence
    /// for a `422 invalid`.
    pub fn resolve(&self) -> Result<Limits, String> {
        let mut out = Limits::default();
        if let Some(v) = &self.cpu {
            out.cpu_millis = Some(cpu_millis(v)?);
        }
        if let Some(v) = &self.memory {
            let b = bytes(v).map_err(|e| format!("bad `limits.memory` {}: {e}", v.show()))?;
            if b < MIN_MEMORY {
                return Err(format!(
                    "bad `limits.memory` {}: give at least 16M",
                    v.show()
                ));
            }
            out.memory_bytes = Some(b);
        }
        if let Some(v) = &self.swap {
            if out.memory_bytes.is_none() {
                return Err("`limits.swap` needs `limits.memory`: swap comes on top of it".into());
            }
            out.swap_bytes =
                Some(bytes(v).map_err(|e| format!("bad `limits.swap` {}: {e}", v.show()))?);
        }
        if let Some(v) = &self.pids {
            match v.whole() {
                Some(n) if (1..=MAX_PIDS).contains(&n) => out.pids = Some(n),
                _ => {
                    return Err(format!(
                        "bad `limits.pids` {}: a whole number from 1 to {MAX_PIDS}",
                        v.show()
                    ));
                }
            }
        }
        if let Some(v) = &self.io_weight {
            match v.whole() {
                Some(n @ 1..=10_000) => out.io_weight = u16::try_from(n).ok(),
                _ => {
                    return Err(format!(
                        "bad `limits.io_weight` {}: a whole number from 1 to 10000",
                        v.show()
                    ));
                }
            }
        }
        Ok(out)
    }
}

fn cpu_millis(v: &LimitValue) -> Result<u32, String> {
    let bad = || {
        format!(
            "bad `limits.cpu` {}: give the cores as a number from 0.01 to 1024, for example 2 \
             or 0.5",
            v.show()
        )
    };
    let cores = v.number().ok_or_else(bad)?;
    if cores <= 0.0 || cores > MAX_CPU {
        return Err(bad());
    }
    let percent = cores * 100.0;
    if (percent - percent.round()).abs() > 1e-6 || percent.round() < 1.0 {
        return Err(format!(
            "bad `limits.cpu` {}: use at most two decimals (0.01 core steps)",
            v.show()
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(percent.round() as u32 * 10)
}

/// A byte count: a whole number, or a number with `K`, `M`, `G`, or `T`
/// (powers of 1024). `B` and `iB` after the unit are allowed: `4GiB`.
pub fn bytes(v: &LimitValue) -> Result<u64, String> {
    let text = match v {
        LimitValue::Int(i) => {
            return u64::try_from(*i).map_err(|_| "not a size".to_owned());
        }
        LimitValue::Float(_) => {
            return Err("give a whole number of bytes, or a text such as \"4G\"".into());
        }
        LimitValue::Text(t) => t.trim(),
    };
    let lower = text.to_ascii_lowercase();
    let body = lower
        .strip_suffix("ib")
        .or_else(|| lower.strip_suffix('b'))
        .unwrap_or(&lower);
    let (num, shift) = match body.chars().last() {
        Some('k') => (&body[..body.len() - 1], 10),
        Some('m') => (&body[..body.len() - 1], 20),
        Some('g') => (&body[..body.len() - 1], 30),
        Some('t') => (&body[..body.len() - 1], 40),
        _ => (body, 0),
    };
    let unit_err = || "use a number with K, M, G, or T, for example \"512M\" or \"4G\"".to_owned();
    let num = num.trim();
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(unit_err());
    }
    if let Ok(n) = num.parse::<u64>() {
        return n
            .checked_shl(shift)
            .filter(|b| b >> shift == n)
            .ok_or_else(|| "too large".into());
    }
    let f: f64 = num.parse().map_err(|_| unit_err())?;
    if shift == 0 {
        return Err("give a whole number of bytes".into());
    }
    #[allow(clippy::cast_precision_loss)]
    let b = f * (1u64 << shift) as f64;
    #[allow(clippy::cast_precision_loss)]
    if !b.is_finite() || b >= u64::MAX as f64 {
        return Err("too large".into());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(b as u64)
}

/// A byte count in the largest unit that holds it whole: `4G`, `1536M`,
/// `100`.
pub fn show_bytes(b: u64) -> String {
    for (shift, unit) in [(40, "T"), (30, "G"), (20, "M"), (10, "K")] {
        if b != 0 && b.trailing_zeros() >= shift {
            return format!("{}{unit}", b >> shift);
        }
    }
    b.to_string()
}

/// Thousandths of a core as cores: `2`, `0.5`, `1.25`.
pub fn show_cores(millis: u32) -> String {
    let whole = millis / 1000;
    let frac = millis % 1000;
    if frac == 0 {
        return whole.to_string();
    }
    let s = format!("{whole}.{frac:03}");
    s.trim_end_matches('0').to_owned()
}

impl Limits {
    /// No limit is set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The cgroup controllers that these limits need on the host.
    pub fn controllers(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.cpu_millis.is_some() {
            v.push("cpu");
        }
        if self.io_weight.is_some() {
            v.push("io");
        }
        if self.memory_bytes.is_some() {
            v.push("memory");
        }
        if self.pids.is_some() {
            v.push("pids");
        }
        v
    }

    /// A short text for a list: `2 CPU · 4G · 512 pids · io 200`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = self.cpu_millis {
            parts.push(format!("{} CPU", show_cores(c)));
        }
        if let Some(m) = self.memory_bytes {
            match self.swap_bytes.filter(|s| *s > 0) {
                Some(s) => parts.push(format!("{} (+{} swap)", show_bytes(m), show_bytes(s))),
                None => parts.push(show_bytes(m)),
            }
        }
        if let Some(p) = self.pids {
            parts.push(format!("{p} pids"));
        }
        if let Some(w) = self.io_weight {
            parts.push(format!("io {w}"));
        }
        parts.join(" · ")
    }

    /// Check that a host agent can apply these limits on a `host` target.
    /// The error completes the sentence "this host agent cannot apply
    /// resource limits: ...".
    pub fn host_support(&self, cap: Option<&LimitsCapability>) -> Result<(), String> {
        let Some(cap) = cap else {
            return Err(too_old());
        };
        if !cap.systemd {
            return Err(cap
                .reason
                .clone()
                .unwrap_or_else(|| "`systemd-run --user` does not work on the host".into()));
        }
        let missing: Vec<&str> = self
            .controllers()
            .into_iter()
            .filter(|c| !cap.controllers.iter().any(|h| h == c))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "the systemd user manager on the host has no {} controller (it has: {}); \
                 delegate it with `Delegate=` in a drop-in of user@.service, or remove the \
                 limits that need it",
                missing.join(", "),
                if cap.controllers.is_empty() {
                    "none".to_owned()
                } else {
                    cap.controllers.join(", ")
                }
            ));
        }
        Ok(())
    }

    /// Check that a host agent can apply these limits on a `docker`
    /// target.
    pub fn docker_support(&self, cap: Option<&LimitsCapability>) -> Result<(), String> {
        match cap {
            None => Err(too_old()),
            Some(c) if !c.docker => Err("Docker is not present on the host".into()),
            Some(_) => Ok(()),
        }
    }
}

fn too_old() -> String {
    "it is older than the limits feature; install the new terrarium-host on the host".into()
}

/// The full refusal for a session on a target with limits.
pub fn refusal(reason: &str) -> String {
    format!("this host agent cannot apply resource limits: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim(text: &str) -> TargetLimits {
        toml::from_str(text).expect("parse limits")
    }

    #[test]
    fn values_resolve_to_numbers() {
        let l = lim("cpu = 2\nmemory = \"4G\"\npids = 512\nio_weight = 200")
            .resolve()
            .unwrap();
        assert_eq!(l.cpu_millis, Some(2000));
        assert_eq!(l.memory_bytes, Some(4 << 30));
        assert_eq!(l.swap_bytes, None);
        assert_eq!(l.pids, Some(512));
        assert_eq!(l.io_weight, Some(200));
        assert_eq!(l.summary(), "2 CPU · 4G · 512 pids · io 200");
        assert_eq!(l.controllers(), ["cpu", "io", "memory", "pids"]);
    }

    #[test]
    fn cpu_takes_fractions_in_whole_percents() {
        let r = |t: &str| lim(t).resolve().map(|l| l.cpu_millis);
        assert_eq!(r("cpu = 0.5"), Ok(Some(500)));
        assert_eq!(r("cpu = \"1.25\""), Ok(Some(1250)));
        assert_eq!(r("cpu = 0.01"), Ok(Some(10)));
        assert!(r("cpu = 0").is_err());
        assert!(r("cpu = -1").is_err());
        assert!(r("cpu = 0.005").is_err());
        assert!(r("cpu = 2000").is_err());
        assert!(r("cpu = \"two\"").is_err());
        assert_eq!(show_cores(1250), "1.25");
        assert_eq!(show_cores(500), "0.5");
        assert_eq!(show_cores(3000), "3");
    }

    #[test]
    fn sizes_take_binary_units() {
        let b = |t: &str| bytes(&LimitValue::Text(t.into()));
        assert_eq!(b("512M"), Ok(512 << 20));
        assert_eq!(b("4g"), Ok(4 << 30));
        assert_eq!(b("4GiB"), Ok(4 << 30));
        assert_eq!(b("4GB"), Ok(4 << 30));
        assert_eq!(b("1.5G"), Ok(1536 << 20));
        assert_eq!(b("1024"), Ok(1024));
        assert_eq!(bytes(&LimitValue::Int(1 << 30)), Ok(1 << 30));
        assert!(b("4X").is_err());
        assert!(b("-4G").is_err());
        assert!(b("G").is_err());
        assert!(b("1.5").is_err());
        assert!(b("99999999999T").is_err());
        assert!(bytes(&LimitValue::Int(-1)).is_err());
        assert_eq!(show_bytes(4 << 30), "4G");
        assert_eq!(show_bytes(1536 << 20), "1536M");
        assert_eq!(show_bytes(1000), "1000");
    }

    #[test]
    fn bad_values_name_the_field() {
        let e = |t: &str| lim(t).resolve().unwrap_err();
        assert!(e("memory = \"4X\"").contains("limits.memory"));
        assert!(e("memory = \"1M\"").contains("at least 16M"));
        assert!(e("swap = \"1G\"").contains("needs `limits.memory`"));
        assert!(e("pids = 0").contains("limits.pids"));
        assert!(e("pids = 1.5").contains("limits.pids"));
        assert!(e("io_weight = 10001").contains("limits.io_weight"));
        assert!(toml::from_str::<TargetLimits>("memroy = \"1G\"").is_err());
    }

    #[test]
    fn swap_shows_in_the_summary() {
        let l = lim("memory = \"2G\"\nswap = \"1G\"").resolve().unwrap();
        assert_eq!(l.swap_bytes, Some(1 << 30));
        assert_eq!(l.summary(), "2G (+1G swap)");
    }

    #[test]
    fn json_round_trip_keeps_the_written_form() {
        let l = lim("cpu = 0.5\nmemory = \"4G\"\npids = 64");
        let j = serde_json::to_value(&l).unwrap();
        assert_eq!(
            j,
            serde_json::json!({"cpu": 0.5, "memory": "4G", "pids": 64})
        );
        let back: TargetLimits = serde_json::from_value(j).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn support_needs_the_capability_and_the_controllers() {
        let l = lim("cpu = 1\nmemory = \"1G\"\nio_weight = 50")
            .resolve()
            .unwrap();
        assert!(l.host_support(None).unwrap_err().contains("older"));
        let off = LimitsCapability {
            systemd: false,
            reason: Some("no user manager".into()),
            ..Default::default()
        };
        assert_eq!(l.host_support(Some(&off)).unwrap_err(), "no user manager");
        let partial = LimitsCapability {
            systemd: true,
            controllers: vec!["memory".into(), "pids".into()],
            ..Default::default()
        };
        let e = l.host_support(Some(&partial)).unwrap_err();
        assert!(e.contains("no cpu, io controller"), "{e}");
        let full = LimitsCapability {
            systemd: true,
            controllers: ["cpu", "io", "memory", "pids"].map(String::from).to_vec(),
            reason: None,
            docker: true,
        };
        assert_eq!(l.host_support(Some(&full)), Ok(()));
        assert_eq!(l.docker_support(Some(&full)), Ok(()));
        assert!(l.docker_support(Some(&partial)).is_err());
        assert!(l.docker_support(None).is_err());
        assert!(refusal("x").starts_with("this host agent cannot apply resource limits: "));
    }
}
