//! Host metrics: CPU, memory, network IO, disk IO, and disk space of this
//! machine, for the `metrics` frame (docs/host-protocol.md, "`metrics`").
//!
//! Linux only. The [`Sampler`] reads `/proc/stat`, `/proc/loadavg`,
//! `/proc/meminfo`, `/proc/net/dev`, `/proc/diskstats`, `/proc/mounts`, and
//! `/sys/block`, and calls `statvfs` for each real filesystem. The counters
//! are totals since boot, so the sampler keeps the last counters and
//! computes each rate from the difference between two samples. The client
//! samples every [`METRICS_INTERVAL_SECS`](crate::protocol::METRICS_INTERVAL_SECS).
//!
//! The filter rules (also in docs/host-protocol.md):
//!
//! - Network: every interface in `/proc/net/dev` except `lo` and the
//!   virtual ones, by name prefix: [`VIRTUAL_IFACES`]. So the traffic of a
//!   container counts one time, on the physical interface.
//! - Disk IO: the whole disks of `/sys/block` (a partition is not there),
//!   except loop, RAM, zram, device-mapper, md RAID, optical, floppy, and
//!   network block devices ([`VIRTUAL_DISKS`]): their IO also shows on the
//!   physical disks under them. A sector in `/proc/diskstats` is always 512
//!   bytes.
//! - Disk space: a mount whose source is a block device (`/dev/...`, not
//!   `/dev/loop*`) or a ZFS dataset, with a type that is not in
//!   [`SKIP_FSTYPES`], and with a mount point that is not under
//!   [`SKIP_MOUNT_ROOTS`]. Network and FUSE filesystems have no `/dev`
//!   source, so they are out too (a `statvfs` on a hung network mount can
//!   block). One entry for each device: the shortest mount point wins (a
//!   btrfs volume with many subvolumes shows one time). At most
//!   [`MAX_FILESYSTEMS`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::protocol::{
    CpuMetrics, DiskIoMetrics, FsMetrics, HostMetrics, MemoryMetrics, NetMetrics,
};

/// Interface name prefixes that are virtual: loopback, container and VM
/// bridges and veths, tunnels, and VPNs.
pub const VIRTUAL_IFACES: &[&str] = &[
    "lo",
    "veth",
    "docker",
    "br-",
    "virbr",
    "tap",
    "tun",
    "vnet",
    "cni",
    "flannel",
    "cali",
    "wg",
    "tailscale",
    "vxlan",
    "podman",
    "lxc",
];

/// Disk name prefixes whose IO also shows on another disk, or that are no
/// disk.
pub const VIRTUAL_DISKS: &[&str] = &["loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd"];

/// Filesystem types that hold no data of their own, or that are always
/// full (read-only images).
pub const SKIP_FSTYPES: &[&str] = &[
    "tmpfs", "devtmpfs", "ramfs", "overlay", "squashfs", "iso9660", "udf", "proc", "sysfs",
    "cgroup", "cgroup2", "nsfs", "autofs",
];

/// Mount points under these roots are not disks the operator manages:
/// system mounts, and the layers and volumes of container runtimes.
pub const SKIP_MOUNT_ROOTS: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/snap",
    "/var/lib/docker",
    "/var/lib/containers",
    "/var/lib/kubelet",
];

/// The most filesystems in one sample.
pub const MAX_FILESYSTEMS: usize = 32;

/// The bytes of one sector in `/proc/diskstats`, on every device.
const SECTOR: u64 = 512;

/// CPU time counters from the `cpu` line of `/proc/stat`, in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub total: u64,
    /// `idle + iowait`.
    pub idle: u64,
    /// The `cpuN` lines: the online cores.
    pub cores: u32,
}

/// Parse `/proc/stat`. `None` when the `cpu` line is missing.
pub fn parse_stat(text: &str) -> Option<CpuTimes> {
    let mut out = None;
    let mut cores = 0u32;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("cpu") => {
                // user nice system idle iowait irq softirq steal. `guest`
                // and `guest_nice` are in `user` and `nice` already.
                let v: Vec<u64> = parts.take(8).filter_map(|p| p.parse().ok()).collect();
                if v.len() < 4 {
                    return None;
                }
                let idle = v[3] + v.get(4).copied().unwrap_or(0);
                out = Some(CpuTimes {
                    total: v.iter().sum(),
                    idle,
                    cores: 0,
                });
            }
            Some(c) if c.starts_with("cpu") && c[3..].bytes().all(|b| b.is_ascii_digit()) => {
                cores += 1;
            }
            _ => {}
        }
    }
    out.map(|c| CpuTimes { cores, ..c })
}

/// The busy share between two samples, 0 to 100.
pub fn cpu_busy_pct(prev: &CpuTimes, cur: &CpuTimes) -> f64 {
    let total = cur.total.saturating_sub(prev.total);
    let idle = cur.idle.saturating_sub(prev.idle).min(total);
    if total == 0 {
        return 0.0;
    }
    ((total - idle) as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
}

/// The 1-minute load average from `/proc/loadavg`.
pub fn parse_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// Parse `/proc/meminfo` (values in kB). Without `MemAvailable` (a kernel
/// before 3.14), available is `MemFree + Buffers + Cached`.
pub fn parse_meminfo(text: &str) -> Option<MemoryMetrics> {
    let mut kv: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        let Some((k, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(n) = rest.split_whitespace().next().and_then(|n| n.parse().ok()) {
            kv.insert(k.trim(), n);
        }
    }
    let total = *kv.get("MemTotal")?;
    let get = |k: &str| kv.get(k).copied().unwrap_or(0);
    let avail = kv
        .get("MemAvailable")
        .copied()
        .unwrap_or_else(|| get("MemFree") + get("Buffers") + get("Cached"))
        .min(total);
    let used = total - avail;
    let swap_total = get("SwapTotal");
    let swap_used = swap_total.saturating_sub(get("SwapFree"));
    Some(MemoryMetrics {
        total_bytes: total * 1024,
        used_bytes: used * 1024,
        used_pct: pct(used, total),
        swap_total_bytes: swap_total * 1024,
        swap_used_bytes: swap_used * 1024,
    })
}

/// True for an interface that [`VIRTUAL_IFACES`] does not name.
pub fn is_physical_iface(name: &str) -> bool {
    !VIRTUAL_IFACES.iter().any(|p| name.starts_with(p))
}

/// Parse `/proc/net/dev`: the received and sent bytes of each physical
/// interface.
pub fn parse_net_dev(text: &str) -> HashMap<String, (u64, u64)> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains('|') || !is_physical_iface(name) {
            continue;
        }
        let v: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect();
        // rx: bytes packets errs drop fifo frame compressed multicast,
        // then tx bytes.
        if v.len() >= 9 {
            out.insert(name.to_owned(), (v[0], v[8]));
        }
    }
    out
}

/// True for a disk whose IO counts: a whole disk, not one of
/// [`VIRTUAL_DISKS`]. `whole` is the names in `/sys/block`; without it a
/// name rule finds the partitions (`sda1`, `nvme0n1p2`, `mmcblk0p1`).
pub fn is_counted_disk(name: &str, whole: Option<&HashSet<String>>) -> bool {
    if VIRTUAL_DISKS.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    match whole {
        Some(set) => set.contains(name),
        None => !is_partition_name(name),
    }
}

fn is_partition_name(name: &str) -> bool {
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return false;
    }
    let stem = &name[..name.len() - digits];
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        // nvme0n1 is a disk, nvme0n1p1 a partition.
        return stem.ends_with('p');
    }
    // sda1, vdb2, xvda1, hda3.
    true
}

/// Parse `/proc/diskstats`: the read and written bytes of each counted
/// disk.
pub fn parse_diskstats(text: &str, whole: Option<&HashSet<String>>) -> HashMap<String, (u64, u64)> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // major minor name reads merged sectors_read ms writes merged
        // sectors_written ...
        if f.len() < 10 || !is_counted_disk(f[2], whole) {
            continue;
        }
        let (Ok(r), Ok(w)) = (f[5].parse::<u64>(), f[9].parse::<u64>()) else {
            continue;
        };
        out.insert(f[2].to_owned(), (r * SECTOR, w * SECTOR));
    }
    out
}

/// One line of `/proc/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub device: String,
    pub mount: String,
    pub fstype: String,
}

/// Undo the octal escapes of `/proc/mounts` (`\040` is a space).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let n = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(n);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            Some(Mount {
                device: unescape(f.next()?),
                mount: unescape(f.next()?),
                fstype: f.next()?.to_owned(),
            })
        })
        .collect()
}

/// True for a filesystem that disk space shows (the rules at the top).
pub fn is_real_fs(m: &Mount) -> bool {
    let source_ok =
        (m.device.starts_with("/dev/") && !m.device.starts_with("/dev/loop")) || m.fstype == "zfs";
    let under = |root: &str| {
        m.mount == root
            || m.mount
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    source_ok
        && !SKIP_FSTYPES.contains(&m.fstype.as_str())
        && !m.fstype.starts_with("fuse.")
        && !SKIP_MOUNT_ROOTS.iter().any(|r| under(r))
}

/// The real filesystems, one for each device, sorted by mount point.
pub fn real_filesystems(mounts: &[Mount]) -> Vec<Mount> {
    let mut by_dev: HashMap<&str, &Mount> = HashMap::new();
    for m in mounts.iter().filter(|m| is_real_fs(m)) {
        by_dev
            .entry(m.device.as_str())
            .and_modify(|cur| {
                if (m.mount.len(), &m.mount) < (cur.mount.len(), &cur.mount) {
                    *cur = m;
                }
            })
            .or_insert(m);
    }
    let mut out: Vec<Mount> = by_dev.into_values().cloned().collect();
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out.truncate(MAX_FILESYSTEMS);
    out
}

/// The space of one filesystem, from `statvfs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FsSpace {
    pub total: u64,
    /// Free blocks, root's reserve included.
    pub free: u64,
    /// Free blocks that a user who is not root can use.
    pub avail: u64,
}

impl FsSpace {
    /// The entry of a sample. `None` for a filesystem of size 0.
    pub fn metrics(&self, m: &Mount) -> Option<FsMetrics> {
        if self.total == 0 {
            return None;
        }
        let used = self.total.saturating_sub(self.free);
        Some(FsMetrics {
            mount: m.mount.clone(),
            fstype: m.fstype.clone(),
            device: m.device.clone(),
            total_bytes: self.total,
            used_bytes: used,
            avail_bytes: self.avail,
            used_pct: pct(used, used + self.avail),
        })
    }
}

/// `statvfs` of a path.
pub fn statvfs(path: &Path) -> Option<FsSpace> {
    let s = nix::sys::statvfs::statvfs(path).ok()?;
    let frag = s.fragment_size() as u64;
    Some(FsSpace {
        total: (s.blocks() as u64).saturating_mul(frag),
        free: (s.blocks_free() as u64).saturating_mul(frag),
        avail: (s.blocks_available() as u64).saturating_mul(frag),
    })
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

/// The sum over every key that is in both maps, per second. A counter that
/// went down (a reset, or a new device with the same name) adds 0.
pub fn rates(
    prev: &HashMap<String, (u64, u64)>,
    cur: &HashMap<String, (u64, u64)>,
    secs: f64,
) -> (f64, f64) {
    if secs <= 0.0 {
        return (0.0, 0.0);
    }
    let (mut a, mut b) = (0u64, 0u64);
    for (k, (ca, cb)) in cur {
        if let Some((pa, pb)) = prev.get(k) {
            a += ca.checked_sub(*pa).unwrap_or(0);
            b += cb.checked_sub(*pb).unwrap_or(0);
        }
    }
    (a as f64 / secs, b as f64 / secs)
}

/// The counters of one sample.
#[derive(Debug, Clone)]
struct Counters {
    at: Instant,
    cpu: Option<CpuTimes>,
    net: Option<HashMap<String, (u64, u64)>>,
    disk: Option<HashMap<String, (u64, u64)>>,
}

type StatFn = Box<dyn Fn(&Path) -> Option<FsSpace> + Send>;

/// Reads the counters and makes [`HostMetrics`] from two samples in a row.
pub struct Sampler {
    proc: PathBuf,
    sys: PathBuf,
    stat: StatFn,
    prev: Option<Counters>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new("/proc", "/sys")
    }
}

impl Sampler {
    /// A sampler that reads `proc` and `sys` (normally `/proc` and `/sys`)
    /// and calls the real `statvfs`.
    pub fn new(proc: impl Into<PathBuf>, sys: impl Into<PathBuf>) -> Self {
        Self::with_statvfs(proc, sys, statvfs)
    }

    /// As [`Sampler::new`], with another `statvfs` (for the tests).
    pub fn with_statvfs(
        proc: impl Into<PathBuf>,
        sys: impl Into<PathBuf>,
        stat: impl Fn(&Path) -> Option<FsSpace> + Send + 'static,
    ) -> Self {
        Self {
            proc: proc.into(),
            sys: sys.into(),
            stat: Box::new(stat),
            prev: None,
        }
    }

    /// True when this machine has the files the sampler reads.
    pub fn available() -> bool {
        cfg!(target_os = "linux") && Path::new("/proc/stat").exists()
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.proc.join(name)).ok()
    }

    fn whole_disks(&self) -> Option<HashSet<String>> {
        let rd = std::fs::read_dir(self.sys.join("block")).ok()?;
        Some(
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
        )
    }

    fn counters(&self, at: Instant) -> Counters {
        let whole = self.whole_disks();
        Counters {
            at,
            cpu: self.read("stat").as_deref().and_then(parse_stat),
            net: self.read("net/dev").as_deref().map(parse_net_dev),
            disk: self
                .read("diskstats")
                .as_deref()
                .map(|t| parse_diskstats(t, whole.as_ref())),
        }
    }

    /// Take the first counters. The next [`Sampler::sample`] gives rates.
    pub fn prime(&mut self) {
        self.prev = Some(self.counters(Instant::now()));
    }

    /// One sample at the time `now`. `None` on the first call: a rate needs
    /// two samples.
    pub fn sample_at(&mut self, now: Instant) -> Option<HostMetrics> {
        let cur = self.counters(now);
        let prev = self.prev.replace(cur.clone())?;
        let secs = cur.at.saturating_duration_since(prev.at).as_secs_f64();
        let cpu = match (prev.cpu, cur.cpu) {
            (Some(p), Some(c)) => Some(CpuMetrics {
                busy_pct: cpu_busy_pct(&p, &c),
                cores: c.cores,
                load1: self
                    .read("loadavg")
                    .as_deref()
                    .and_then(parse_loadavg)
                    .unwrap_or(0.0),
            }),
            _ => None,
        };
        let net = match (&prev.net, &cur.net) {
            (Some(p), Some(c)) => {
                let (rx, tx) = rates(p, c, secs);
                Some(NetMetrics {
                    rx_bytes_per_sec: rx,
                    tx_bytes_per_sec: tx,
                })
            }
            _ => None,
        };
        let disk_io = match (&prev.disk, &cur.disk) {
            (Some(p), Some(c)) => {
                let (r, w) = rates(p, c, secs);
                Some(DiskIoMetrics {
                    read_bytes_per_sec: r,
                    write_bytes_per_sec: w,
                })
            }
            _ => None,
        };
        let filesystems = self
            .read("mounts")
            .map(|t| {
                real_filesystems(&parse_mounts(&t))
                    .iter()
                    .filter_map(|m| (self.stat)(Path::new(&m.mount))?.metrics(m))
                    .collect()
            })
            .unwrap_or_default();
        Some(HostMetrics {
            gpus: vec![],
            gpu_usage: vec![],
            interval_secs: secs,
            cpu,
            memory: self.read("meminfo").as_deref().and_then(parse_meminfo),
            net,
            disk_io,
            filesystems,
        })
    }

    /// [`Sampler::sample_at`] now.
    pub fn sample(&mut self) -> Option<HostMetrics> {
        self.sample_at(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/proc");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(Path::new(FIX).join(name)).unwrap()
    }

    #[test]
    fn stat_gives_the_cpu_totals_and_the_cores() {
        let c = parse_stat(&fixture("a/stat")).unwrap();
        assert_eq!(c.cores, 4);
        // user nice system idle iowait irq softirq steal.
        assert_eq!(c.total, 1000 + 10 + 300 + 8000 + 200 + 20 + 10);
        assert_eq!(c.idle, 8000 + 200);
        let next = parse_stat(&fixture("b/stat")).unwrap();
        // 1000 more ticks, 750 of them idle or iowait.
        assert!((cpu_busy_pct(&c, &next) - 25.0).abs() < 1e-9);
        assert_eq!(cpu_busy_pct(&next, &next), 0.0);
        assert_eq!(parse_stat("intr 1 2 3"), None);
    }

    #[test]
    fn loadavg_and_meminfo() {
        assert_eq!(parse_loadavg(&fixture("a/loadavg")), Some(1.25));
        let m = parse_meminfo(&fixture("a/meminfo")).unwrap();
        assert_eq!(m.total_bytes, 16_000_000 * 1024);
        assert_eq!(m.used_bytes, 4_000_000 * 1024);
        assert!((m.used_pct - 25.0).abs() < 1e-9);
        assert_eq!(m.swap_total_bytes, 2_000_000 * 1024);
        assert_eq!(m.swap_used_bytes, 500_000 * 1024);
        // An old kernel without MemAvailable.
        let old =
            parse_meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 250 kB\n")
                .unwrap();
        assert_eq!(old.used_bytes, 600 * 1024);
        assert_eq!(parse_meminfo("MemFree: 1 kB"), None);
    }

    #[test]
    fn net_dev_keeps_the_physical_interfaces() {
        let n = parse_net_dev(&fixture("a/net/dev"));
        let mut names: Vec<&str> = n.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["enp3s0", "wlan0"]);
        assert_eq!(n["enp3s0"], (1_000_000, 500_000));
        for v in [
            "lo",
            "veth0aac2e2",
            "docker0",
            "br-32cd90606f27",
            "virbr0",
            "tap0",
            "tun0",
        ] {
            assert!(!is_physical_iface(v), "{v}");
        }
        for v in ["vnet3", "cni0", "flannel.1", "cali123", "wg0", "tailscale0"] {
            assert!(!is_physical_iface(v), "{v}");
        }
        for p in ["eth0", "enp3s0", "wlan0", "eno1", "bond0"] {
            assert!(is_physical_iface(p), "{p}");
        }
    }

    #[test]
    fn diskstats_counts_whole_disks_only() {
        let whole: HashSet<String> = ["nvme0n1", "sda", "dm-0", "zram0", "loop0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let d = parse_diskstats(&fixture("a/diskstats"), Some(&whole));
        let mut names: Vec<&str> = d.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["nvme0n1", "sda"]);
        assert_eq!(d["sda"], (2000 * 512, 4000 * 512));
        // Without /sys/block, the name rule finds the partitions.
        let d = parse_diskstats(&fixture("a/diskstats"), None);
        let mut names: Vec<&str> = d.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["nvme0n1", "sda"]);
        for (n, part) in [
            ("sda1", true),
            ("nvme0n1p2", true),
            ("mmcblk0p1", true),
            ("nvme0n1", false),
            ("mmcblk0", false),
            ("vda", false),
        ] {
            assert_eq!(is_partition_name(n), part, "{n}");
        }
    }

    #[test]
    fn mounts_keep_one_real_filesystem_per_device() {
        let mounts = parse_mounts(&fixture("a/mounts"));
        assert!(
            mounts.iter().any(|m| m.device == "Google Drive:"),
            "\\040 is a space"
        );
        let real = real_filesystems(&mounts);
        let got: Vec<(&str, &str)> = real
            .iter()
            .map(|m| (m.mount.as_str(), m.fstype.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/", "btrfs"),
                ("/boot", "vfat"),
                ("/data", "ext4"),
                ("/tank", "zfs")
            ]
        );
        let m = |device: &str, mount: &str, fstype: &str| Mount {
            device: device.into(),
            mount: mount.into(),
            fstype: fstype.into(),
        };
        assert!(!is_real_fs(&m("tmpfs", "/tmp", "tmpfs")));
        assert!(!is_real_fs(&m("/dev/loop3", "/snap/core/1", "squashfs")));
        assert!(!is_real_fs(&m("nas:/vol", "/mnt/nas", "nfs4")));
        assert!(!is_real_fs(&m("/dev/sdb1", "/var/lib/docker/x", "ext4")));
        assert!(!is_real_fs(&m(
            "tank/docker/abc",
            "/var/lib/docker/zfs/abc",
            "zfs"
        )));
        assert!(is_real_fs(&m("/dev/sdb1", "/var/lib/dockerdata", "ext4")));
        assert!(is_real_fs(&m("/dev/sdc1", "/media/usb", "fuseblk")));
    }

    #[test]
    fn statvfs_space_follows_df() {
        let s = FsSpace {
            total: 1000,
            free: 400,
            avail: 300,
        };
        let f = s
            .metrics(&Mount {
                device: "/dev/sda1".into(),
                mount: "/".into(),
                fstype: "ext4".into(),
            })
            .unwrap();
        assert_eq!(f.used_bytes, 600);
        // 600 / (600 + 300): the reserve for root does not count.
        assert!((f.used_pct - 66.666).abs() < 0.01);
        assert!(
            FsSpace::default()
                .metrics(&Mount {
                    device: "/dev/x".into(),
                    mount: "/x".into(),
                    fstype: "ext4".into(),
                })
                .is_none()
        );
    }

    #[test]
    fn rates_are_per_second_and_ignore_resets() {
        let m = |v: &[(&str, u64, u64)]| -> HashMap<String, (u64, u64)> {
            v.iter()
                .map(|(k, a, b)| (k.to_string(), (*a, *b)))
                .collect()
        };
        let prev = m(&[("eth0", 1000, 100), ("eth1", 5000, 5000)]);
        // eth1 went back to 0 (a reset); eth2 is new.
        let cur = m(&[("eth0", 3000, 600), ("eth1", 10, 10), ("eth2", 9999, 9999)]);
        assert_eq!(rates(&prev, &cur, 10.0), (200.0, 50.0));
        assert_eq!(rates(&prev, &cur, 0.0), (0.0, 0.0));
    }

    #[test]
    fn sampler_computes_rates_from_two_fixture_trees() {
        let dir = tempfile::tempdir().unwrap();
        let proc = dir.path().join("proc");
        let sys = dir.path().join("sys");
        std::fs::create_dir_all(proc.join("net")).unwrap();
        for d in ["nvme0n1", "sda", "dm-0"] {
            std::fs::create_dir_all(sys.join("block").join(d)).unwrap();
        }
        let copy = |tree: &str| {
            for f in [
                "stat",
                "loadavg",
                "meminfo",
                "net/dev",
                "diskstats",
                "mounts",
            ] {
                std::fs::copy(Path::new(FIX).join(tree).join(f), proc.join(f)).unwrap();
            }
        };
        let mut s = Sampler::with_statvfs(&proc, &sys, |p| {
            (p == Path::new("/")).then_some(FsSpace {
                total: 100 << 30,
                free: 10 << 30,
                avail: 10 << 30,
            })
        });
        copy("a");
        let t0 = Instant::now();
        assert!(s.sample_at(t0).is_none(), "the first call only primes");
        copy("b");
        let m = s.sample_at(t0 + Duration::from_secs(10)).unwrap();
        assert_eq!(m.interval_secs, 10.0);
        let cpu = m.cpu.unwrap();
        assert!((cpu.busy_pct - 25.0).abs() < 1e-9);
        assert_eq!((cpu.cores, cpu.load1), (4, 1.5));
        let net = m.net.unwrap();
        // enp3s0 +100000/+20000, wlan0 +50000/+10000 over 10 s.
        assert_eq!(
            (net.rx_bytes_per_sec, net.tx_bytes_per_sec),
            (15_000.0, 3_000.0)
        );
        let io = m.disk_io.unwrap();
        // sda +1000/+2000 sectors, nvme0n1 +0/+2000 sectors.
        assert_eq!(io.read_bytes_per_sec, 1000.0 * 512.0 / 10.0);
        assert_eq!(io.write_bytes_per_sec, 4000.0 * 512.0 / 10.0);
        assert!(m.memory.is_some());
        // Only `/` has a statvfs answer in this test.
        assert_eq!(m.filesystems.len(), 1);
        assert_eq!(m.filesystems[0].mount, "/");
        assert!((m.filesystems[0].used_pct - 90.0).abs() < 1e-9);
    }

    #[test]
    fn missing_files_leave_the_parts_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Sampler::with_statvfs(dir.path(), dir.path(), |_| None);
        s.prime();
        let m = s.sample().unwrap();
        assert!(m.cpu.is_none() && m.memory.is_none() && m.net.is_none() && m.disk_io.is_none());
        assert!(m.filesystems.is_empty());
    }

    /// Reads this machine's real `/proc`. Run with `--ignored --nocapture`
    /// to see one sample.
    #[test]
    #[ignore = "reads the real /proc of this machine"]
    fn real_proc_sample() {
        let mut s = Sampler::default();
        s.prime();
        std::thread::sleep(Duration::from_secs(2));
        let m = s.sample().unwrap();
        eprintln!("{}", serde_json::to_string_pretty(&m).unwrap());
        let cpu = m.cpu.unwrap();
        assert!(cpu.cores > 0 && (0.0..=100.0).contains(&cpu.busy_pct));
        assert!(m.memory.unwrap().total_bytes > 0);
        assert!(!m.filesystems.is_empty());
    }
}
