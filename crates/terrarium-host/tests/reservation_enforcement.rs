use std::collections::BTreeMap;
use terrarium_core::limits::{Limits, LimitsCapability};
use terrarium_core::resources::{IoAmounts, RuntimeLimits};
use terrarium_host::{docker, limits};

fn budget() -> RuntimeLimits {
    RuntimeLimits {
        limits: Limits { cpu_millis: Some(2000), memory_bytes: Some(1 << 30), ..Default::default() },
        cpus: Some(vec![1, 3]),
        io: BTreeMap::from([("8:0".into(), IoAmounts { read_bps: 1024, write_iops: 50, ..Default::default() })]),
    }
}

#[test]
fn create_has_device_limits_but_live_update_refuses_them() {
    let b = budget();
    let flags = docker::reservation_flags(&b);
    assert!(flags.contains(&"--cpuset-cpus=1,3".into()));
    assert!(flags.contains(&"--device-read-bps=/dev/block/8:0:1024".into()));
    assert!(flags.contains(&"--device-write-iops=/dev/block/8:0:50".into()));
    let err = docker::plan_update("container", &b, "docker").unwrap_err();
    assert!(err.message.contains("next up"));
}

#[test]
fn live_update_sets_supported_fields_without_recreating() {
    let mut b = budget();
    b.io.clear();
    let plan = docker::plan_update("container", &b, "fake-docker").unwrap();
    assert_eq!(plan.exec.program, "fake-docker");
    assert_eq!(plan.exec.args[0], "update");
    assert_eq!(plan.exec.args.last().unwrap(), "container");
    assert!(plan.exec.args.contains(&"--cpus=2".into()));
    assert!(plan.exec.args.contains(&"--memory=1073741824".into()));
    assert!(plan.exec.args.contains(&"--cpuset-cpus=1,3".into()));
}

#[test]
fn default_update_explicitly_clears_old_limits() {
    let args = docker::plan_update("container", &RuntimeLimits::default(), "docker").unwrap().exec.args;
    for flag in ["--cpus=0", "--memory=0", "--memory-swap=-1", "--pids-limit=-1", "--blkio-weight=0", "--cpuset-cpus="] {
        assert!(args.contains(&flag.into()), "{flag}");
    }
}

#[test]
fn slice_device_limits_are_live_and_clear_absent_dimensions() {
    let p = limits::reservation_properties(&budget()).unwrap();
    for prop in ["AllowedCPUs=1,3", "IOReadBandwidthMax=/dev/block/8:0 1024", "IOWriteBandwidthMax=", "IOReadIOPSMax=", "IOWriteIOPSMax=/dev/block/8:0 50"] {
        assert!(p.contains(&prop.into()), "{prop}");
    }
    let plan = limits::reservation_plan("target", "project/agent", &budget()).unwrap();
    assert_eq!(plan.exec.program, "systemctl");
    assert!(plan.exec.args.contains(&"terrarium-target-project-agent.slice".into()));
}

#[test]
fn affinity_and_io_require_controllers_and_safe_devices() {
    let mut cap = LimitsCapability { systemd: true, controllers: vec!["cpu".into(), "memory".into()], ..Default::default() };
    let mut b = budget();
    assert!(b.host_support(Some(&cap)).unwrap_err().contains("cpuset"));
    cap.controllers.push("cpuset".into());
    assert!(b.host_support(Some(&cap)).unwrap_err().contains("io"));
    cap.controllers.push("io".into());
    assert!(b.host_support(Some(&cap)).is_ok());
    b.io.insert("8:0;touch /tmp/bad".into(), IoAmounts::default());
    assert!(limits::reservation_properties(&b).is_err());
    b.io.clear();
    b.cpus = Some(vec![]);
    assert!(b.validate().is_err());
}
