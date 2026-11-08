use std::fs;
use terrarium_host::capabilities::detect_capacity;

#[test]
fn capacity_reports_all_logical_cpu_ids_and_total_memory() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("stat"), "cpu 100 20\ncpu0 40 10\ncpu7 60 10\nintr 100\n").unwrap();
    fs::write(root.path().join("meminfo"), "MemTotal: 1024 kB\nMemFree: 128 kB\n").unwrap();
    let capacity = detect_capacity(root.path()).unwrap();
    assert_eq!(capacity.cpu_ids, [0, 7]);
    assert_eq!(capacity.memory_bytes, 1024 * 1024);
}

#[test]
fn unavailable_or_invalid_capacity_is_not_guessed() {
    let root = tempfile::tempdir().unwrap();
    assert!(detect_capacity(root.path()).is_none());
    fs::write(root.path().join("stat"), "cpu0 1\n").unwrap();
    for mem in ["MemFree: 1 kB", "MemTotal: 0 kB", "MemTotal: 18446744073709551615 kB", "MemTotal: 1024 B"] {
        fs::write(root.path().join("meminfo"), mem).unwrap();
        assert!(detect_capacity(root.path()).is_none(), "{mem}");
    }
}

#[test]
fn old_host_capabilities_have_no_capacity() {
    let cap: terrarium_core::protocol::HostCapabilities = serde_json::from_str("{}").unwrap();
    assert!(cap.capacity.is_none());
}
