//! Invoked only as an exact-filter child by the disposable interrupted-state test.
#[test]
#[ignore = "internal aborting probe; run through the disposable parent fixture"]
fn run() {
    super::disposable::run_crash_child();
}
