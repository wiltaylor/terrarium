// TER-611 negative proof only, never merged: passes as root, fails as uid 1001.
#[test]
fn ter611_it_needs_root() {
    std::fs::read_dir("/root").expect("read /root");
}
