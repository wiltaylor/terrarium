use std::collections::BTreeMap;
use terrarium_core::resources::{Amounts, HostCapacity, IoAmounts};

#[test]
fn percentage_rounds_down_without_overflow() {
    let cap = Amounts { cpu_millis: 1001, memory_bytes: u64::MAX, ..Default::default() };
    assert_eq!(cap.reservable(50).unwrap().cpu_millis, 500);
    assert_eq!(cap.reservable(100).unwrap(), cap);
    assert_eq!(cap.reservable(50).unwrap().memory_bytes, u64::MAX / 2);
    assert!(cap.reservable(0).unwrap().is_empty());
    assert!(cap.reservable(101).is_err());
}

#[test]
fn admission_checks_each_dimension_and_preserves_original() {
    let cap = Amounts { cpu_millis: 2000, memory_bytes: 4096, ..Default::default() };
    let req = Amounts { cpu_millis: 1000, memory_bytes: 4096, ..Default::default() };
    let remaining = cap.remaining_after(&req).unwrap();
    assert_eq!(remaining.cpu_millis, 1000);
    assert_eq!(remaining.memory_bytes, 0);
    assert!(remaining.remaining_after(&req).is_none());
    assert_eq!(cap.memory_bytes, 4096);
    assert!(cap.remaining_after(&Amounts { cpu_millis: u64::MAX, ..Default::default() }).is_none());
}

#[test]
fn io_admission_requires_known_device_and_every_dimension() {
    let mut cap = Amounts { io: BTreeMap::from([("8:0".into(), IoAmounts {
        read_bps: 100, write_bps: 100, read_iops: 10, write_iops: 10,
    })]), ..Default::default() };
    assert!(cap.remaining_after(&cap).unwrap().is_empty());
    let req = cap.clone();
    cap.io.get_mut("8:0").unwrap().write_iops = 9;
    assert!(cap.remaining_after(&req).is_none());
    cap.io.clear();
    assert!(cap.remaining_after(&req).is_none());
}

#[test]
fn device_names_have_one_canonical_representation() {
    for device in ["/dev/sda", "8:00", "+8:0", "8:0:1", "8:-1", "4294967296:0"] {
        let a = Amounts { io: BTreeMap::from([(device.into(), IoAmounts::default())]), ..Default::default() };
        assert!(a.validate().is_err(), "{device}");
    }
    assert!(Amounts { io: BTreeMap::from([("8:0".into(), IoAmounts::default())]), ..Default::default() }.validate().is_ok());
}

#[test]
fn capacity_counts_distinct_logical_cores() {
    let c = HostCapacity { cpu_ids: vec![0, 2, 2, 7], memory_bytes: 4096 };
    assert_eq!(c.amounts().cpu_millis, 3000);
}
