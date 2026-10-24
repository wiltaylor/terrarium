//! `?wait=<seconds>` on list endpoints (`docs/agent-rpc.md`). The agent CLI
//! cannot open a WebSocket or an SSE stream through its runner socket, so
//! the `wait` commands hold a list request open instead.

use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use super::Shared;
use crate::error::ApiResult;
use crate::store::Paged;

/// The longest hold, in seconds. A larger `wait` is cut to this.
pub const MAX_WAIT_SECS: u64 = 300;

/// `?wait=<seconds>`.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct WaitQuery {
    pub wait: Option<u64>,
}

impl WaitQuery {
    /// The hold time, or `None` for an answer at once.
    pub fn duration(self) -> Option<Duration> {
        self.wait
            .filter(|s| *s > 0)
            .map(|s| Duration::from_secs(s.min(MAX_WAIT_SECS)))
    }
}

/// Run `query`. When the page is empty and `wait` is set, run it again after
/// each new event of one of `kinds`, until the page has items or the time is
/// up. A timeout answers the empty page.
///
/// The hub subscription starts before the first query, so an event that
/// commits between the query and the wait still wakes the request.
pub async fn hold<T, F, Fut>(
    state: &Shared,
    wait: WaitQuery,
    kinds: &[&str],
    mut query: F,
) -> ApiResult<Paged<T>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ApiResult<Paged<T>>>,
{
    let Some(limit) = wait.duration() else {
        return query().await;
    };
    let deadline = tokio::time::Instant::now() + limit;
    let mut rx = state.db.hub().subscribe();
    loop {
        let page = query().await?;
        if !page.items.is_empty() {
            return Ok(page);
        }
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(e) if kinds.contains(&e.kind.as_str()) => break,
                    Ok(_) => continue,
                    // Missed events: query again to be sure.
                    Err(RecvError::Lagged(_)) => break,
                    Err(RecvError::Closed) => return Ok(page),
                },
                () = tokio::time::sleep_until(deadline) => return Ok(page),
            }
        }
    }
}
