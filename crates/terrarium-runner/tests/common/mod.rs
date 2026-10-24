//! Shared by the integration tests: the timeouts, scaled for slow machines.
//!
//! The tests wait for conditions, not for fixed times. Each wait has a
//! generous limit, so a loaded machine only makes a test slower. Set
//! `TERRARIUM_TEST_SLOW=1` (CI) to multiply every limit and every timing
//! setting by 4, or `TERRARIUM_TEST_SLOW=<n>` to multiply by `n`.

#![allow(dead_code)]

use std::time::Duration;

/// The factor from `TERRARIUM_TEST_SLOW`.
pub fn slow_factor() -> u32 {
    match std::env::var("TERRARIUM_TEST_SLOW").ok().as_deref() {
        None | Some("" | "0") => 1,
        Some("1" | "true" | "yes") => 4,
        Some(n) => n.parse().unwrap_or(4).max(1),
    }
}

/// `d`, times the slow factor.
pub fn scaled(d: Duration) -> Duration {
    d * slow_factor()
}

/// `ms` milliseconds, times the slow factor.
pub fn ms(ms: u64) -> Duration {
    scaled(Duration::from_millis(ms))
}

/// The limit of one wait for a condition.
pub fn wait() -> Duration {
    scaled(Duration::from_secs(30))
}

/// Poll `cond` every 20 ms until it is true. Panics with `what` after
/// [`wait`].
pub async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let limit = wait();
    let res = tokio::time::timeout(limit, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(res.is_ok(), "timed out after {limit:?}: {what}");
}
