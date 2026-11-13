//! Target leases at run time (docs/api.md §17.7): grant, extend, revoke,
//! and the expiry timer. The rows and their rules are in
//! [`crate::store::leases`].
//!
//! - A grant to a running agent brings an ephemeral target up
//!   ([`executor::ensure_up`], with `snapshot_before_task`), unless another
//!   running agent holds it already.
//! - When a lease ends (a revoke or the expiry), the daemon ends the
//!   agent's live exec sessions on the target (`lease_revoked`), unless its
//!   `policy.targets` lists the target. When the agent ran and no other
//!   running agent holds the target, the target goes down
//!   ([`executor::maybe_down`], with the keep and restore rules).
//! - Each change writes a `lease` event to the agent's stream. The
//!   supervisor types the "ended" pointer from it. The grant pointer comes
//!   from [`grant`] after the lifecycle step ([`crate::supervisor::Supervisor::lease_granted`]),
//!   so the agent never sees a target that is still coming up. An extend
//!   types no pointer.
//! - The timer: one task for each lease with an expiry, at that time, and a
//!   sweep every [`SWEEP`] that also catches leases that ended while the
//!   daemon was down. Tests set another sweep interval ([`Sweep`]).

use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::actor::Actor;
use crate::api::{AppState, Shared};
use crate::error::{ApiError, ApiResult};
use crate::executor;
use crate::policy;
use crate::store::leases::{self, Grant, LeaseRow};
use crate::store::{agents, targets};

/// How often the timer looks for leases past their expiry.
pub const SWEEP: Duration = Duration::from_secs(10);

/// The interval of the lease sweep (`AppState::lease_sweep`): [`SWEEP`]
/// unless a test sets another.
#[derive(Debug, Default)]
pub struct Sweep(std::sync::Mutex<Option<Duration>>);

impl Sweep {
    /// The interval of the sweep.
    pub fn every(&self) -> Duration {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(SWEEP)
    }

    /// Tests: another interval. It applies within a second. A long one
    /// leaves the expiry to the timer of each lease.
    pub fn set_every(&self, d: Duration) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
    }
}

/// The end code of an exec session whose lease ended.
pub const LEASE_REVOKED: &str = "lease_revoked";

/// The body of `POST /api/targets/{name}/leases`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantBody {
    /// The agent: a ULID, `project/name`, or a name.
    pub agent: String,
    /// An RFC 3339 time, or a duration from now (`30m`, `2h`, `1d`).
    /// Absent or `null`: until a revoke.
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub exclusive: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A lease, with the lifecycle verb that a grant ran.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseOut {
    #[serde(flatten)]
    pub lease: LeaseRow,
    /// The report of `up` (or the snapshot) that the grant ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Value>,
    /// The grant stands, but the target did not come up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_error: Option<String>,
}

/// Parse `until`: an RFC 3339 time in the future, or a duration from now.
/// Returns the stored form (UTC, milliseconds).
pub fn parse_until(until: Option<&str>) -> ApiResult<Option<String>> {
    let Some(u) = until.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let now = chrono::Utc::now();
    let at = match chrono::DateTime::parse_from_rfc3339(u) {
        Ok(t) => t.with_timezone(&chrono::Utc),
        Err(_) => {
            let d = crate::settings::parse_duration(u).map_err(|e| {
                ApiError::invalid(format!(
                    "bad `until` `{u}`: an RFC 3339 time or a duration such as 30m ({e})"
                ))
            })?;
            now + chrono::Duration::from_std(d)
                .map_err(|_| ApiError::invalid(format!("`until` `{u}` is too far away")))?
        }
    };
    if at <= now {
        return Err(ApiError::invalid(format!(
            "`until` `{u}` is not in the future"
        )));
    }
    Ok(Some(
        at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    ))
}

/// The target and the agent of a lease call, and the rights check.
async fn load(
    state: &Shared,
    actor: &Actor,
    target: &str,
    agent_key: &str,
) -> ApiResult<(targets::TargetRow, agents::AgentView)> {
    let (t, k) = (target.to_owned(), agent_key.to_owned());
    let context = actor.project().map(str::to_owned);
    let (row, agent) = state
        .db
        .read(move |c| {
            let row = targets::get(c, &t)?;
            let ctx = row.def.project.clone().or(context);
            let agent = agents::find(c, &k, ctx.as_deref())?;
            Ok((row, agent))
        })
        .await?;
    Ok((row, agent))
}

async fn is_running(state: &Shared, agent: &agents::AgentView) -> bool {
    state
        .supervisor
        .status(agent.agent.id)
        .await
        .is_some_and(|s| s.is_live() || s == terrarium_core::AgentStatus::Restarting)
}

/// `POST /api/targets/{name}/leases`.
pub async fn grant(
    state: &Shared,
    actor: &Actor,
    target: &str,
    body: GrantBody,
) -> ApiResult<LeaseOut> {
    let (row, agent) = load(state, actor, target, &body.agent).await?;
    policy::lease_target(
        actor,
        target,
        row.def.project.as_deref(),
        &agent.agent.project,
    )?;
    if body.exclusive && row.def.lab_from_workspace {
        return Err(ApiError::invalid(format!(
            "`{target}` gives each holder its own lab instance, so an exclusive lease does \
             not apply; grant it without `exclusive`"
        )));
    }
    let g = Grant {
        target: row.def.name.clone(),
        target_project: row.def.project.clone(),
        agent: agent.agent.id.to_string(),
        agent_project: agent.agent.project.clone(),
        expires_at: parse_until(body.until.as_deref())?,
        exclusive: body.exclusive,
        reason: body.reason.filter(|r| !r.trim().is_empty()),
    };
    let lease = state
        .db
        .write(actor.label(), move |t| leases::grant(t, &g))
        .await?;
    if let Some(at) = &lease.expires_at {
        schedule(state, at);
    }
    let mut out = LeaseOut {
        lease,
        lifecycle: None,
        lifecycle_error: None,
    };
    // A running agent gets the target up now, as at its start: its own
    // instance of a `lab_from_workspace` target; else the target, or its
    // workspace joins the target that other holders run.
    let in_policy = agent.agent.policy.targets.contains(&row.def.name);
    if !in_policy && is_running(state, &agent).await {
        let id = agent.agent.id.to_string();
        let res = if row.def.lab_from_workspace {
            executor::ensure_up_as(state, target, "lease_grant", Some(&id)).await
        } else {
            match executor::running_holders(state, target, Some(&id)).await {
                Ok(h) if h.is_empty() => {
                    executor::ensure_up_as(state, target, "lease_grant", Some(&id)).await
                }
                Ok(_) => executor::attach_join(state, target, &id, "lease_grant").await,
                Err(e) => Err(e),
            }
        };
        match res {
            Ok(v) => {
                if let Some(e) = v.as_ref().and_then(|v| v["attach_error"].as_str()) {
                    out.lifecycle_error = Some(e.to_owned());
                }
                out.lifecycle = v;
            }
            Err(e) => {
                warn!(target = %target, "the leased target did not come up: {e}");
                out.lifecycle_error = Some(e.message);
            }
        }
    }
    // The pointer, now that the target is up (or did not come up). A
    // revoke during the lifecycle step leaves only its "ended" pointer.
    let id = out.lease.id.clone();
    let active = state
        .db
        .read(move |c| leases::get(c, &id))
        .await
        .is_ok_and(|l| l.is_active());
    if active {
        state
            .supervisor
            .lease_granted(agent.agent.id, row.def.name.clone(), out.lease.id.clone());
    }
    Ok(out)
}

/// The grant pointer line (docs/api.md §17.7). `state` is the target state
/// that the holder sees.
pub fn granted_line(target: &str, state: &str, until: Option<&str>) -> String {
    let state = match state {
        "running" => "running".to_owned(),
        "stopped" | "absent" => format!("{state}: ask your lead to bring it up"),
        s => s.to_owned(),
    };
    format!(
        "You now have access to target {target} ({state}) until {}. Run `terrarium targets ls`.",
        until.unwrap_or("revoked")
    )
}

/// The pointer line of a lease that ended.
pub fn ended_line(target: &str) -> String {
    format!("Your access to target {target} ended.")
}

/// The grant pointer line of lease `id` now: the target state (of the
/// holder's own instance for a `lab_from_workspace` target) and the expiry
/// now. `None` when the lease is no longer active.
pub fn pointer_line(conn: &rusqlite::Connection, id: &str) -> ApiResult<Option<String>> {
    let l = leases::get(conn, id)?;
    if !l.is_active() {
        return Ok(None);
    }
    let t = targets::get(conn, &l.target)?;
    let state = if t.def.lab_from_workspace {
        t.runtime
            .instances
            .get(&l.agent)
            .map_or("absent", |i| i.state.as_str())
            .to_owned()
    } else {
        t.state
    };
    let mut line = granted_line(&l.target, &state, l.expires_at.as_deref());
    if crate::display::has_display(&t.def) {
        line.push(' ');
        line.push_str(&display_hint(&l.target, l.exclusive));
    }
    Ok(Some(line))
}

/// The computer-use hint of a grant on a target with a screen
/// (docs/api.md §17.10).
pub fn display_hint(target: &str, exclusive: bool) -> String {
    let input = if exclusive {
        "then act with `click`, `keys`, or `type`, and take a new shot"
    } else {
        "input needs an exclusive lease"
    };
    format!(
        "It has a screen: run `terrarium screen {target} shot`, open the PNG it prints, {input}. Keep the actions few."
    )
}

/// `DELETE /api/targets/{name}/leases/{agent}`. A lease holder may give
/// its own lease back.
pub async fn revoke(
    state: &Shared,
    actor: &Actor,
    target: &str,
    agent_key: &str,
) -> ApiResult<LeaseRow> {
    let (row, agent) = load(state, actor, target, agent_key).await?;
    let own = actor
        .agent()
        .is_some_and(|a| a.id == agent.agent.id.to_string());
    if !own {
        policy::lease_target(
            actor,
            target,
            row.def.project.as_deref(),
            &agent.agent.project,
        )?;
    }
    let (t, a) = (row.def.name.clone(), agent.agent.id.to_string());
    let lease = state
        .db
        .write(actor.label(), move |tx| leases::revoke(tx, &t, &a))
        .await?;
    after_end(state, &lease).await;
    Ok(lease)
}

/// `PATCH /api/targets/{name}/leases/{agent}`: a new `until`.
pub async fn extend(
    state: &Shared,
    actor: &Actor,
    target: &str,
    agent_key: &str,
    until: Option<String>,
) -> ApiResult<LeaseRow> {
    let (row, agent) = load(state, actor, target, agent_key).await?;
    policy::lease_target(
        actor,
        target,
        row.def.project.as_deref(),
        &agent.agent.project,
    )?;
    let at = parse_until(until.as_deref())?;
    let (t, a) = (row.def.name.clone(), agent.agent.id.to_string());
    let lease = state
        .db
        .write(actor.label(), move |tx| leases::extend(tx, &t, &a, at))
        .await?;
    if let Some(at) = &lease.expires_at {
        schedule(state, at);
    }
    Ok(lease)
}

/// Mark the leases past their expiry as `expired`, and end their use.
pub async fn expire(state: &Shared) -> ApiResult<Vec<LeaseRow>> {
    let ended = state.db.write("system", leases::expire_due).await?;
    for l in &ended {
        info!(target = %l.target, agent = %l.holder(), "lease expired");
        after_end(state, l).await;
    }
    Ok(ended)
}

/// The end of a lease: end the agent's sessions on the target, unless its
/// policy lists the target; then bring the target down when the agent ran
/// and no other running agent holds it.
pub async fn after_end(state: &Shared, l: &LeaseRow) {
    // Live attachment authority is fenced before either legacy shortcut below.
    // A stopped/deleted holder still needs ordered path-revoke processing.
    let (holder, target) = (l.agent.clone(), l.target.clone());
    if let Err(e) = state
        .db
        .write_full("system", move |t| {
            crate::store::live_workspaces::end_holder_authority(t, &holder, Some(&target), false)
        })
        .await
    {
        warn!(agent = %l.agent, target = %l.target, "cannot fence live workspace authority after lease end: {e}");
    }
    let a = l.agent.clone();
    let agent = match state.db.read(move |c| agents::get(c, &a)).await {
        Ok(v) => Some(v),
        Err(e) => {
            debug!(agent = %l.agent, "the agent of an ended lease: {e}");
            None
        }
    };
    if agent
        .as_ref()
        .is_some_and(|v| v.agent.policy.targets.contains(&l.target))
    {
        return;
    }
    let n = state.executor.end_sessions(
        &l.target,
        &l.agent,
        LEASE_REVOKED,
        &format!("your lease on {} ended", l.target),
    );
    if n > 0 {
        info!(target = %l.target, agent = %l.holder(), sessions = n, "ended the sessions of a lease");
    }
    let ran = match &agent {
        Some(v) => is_running(state, v).await,
        None => false,
    };
    if !ran {
        return;
    }
    let t = l.target.clone();
    let per_holder = state
        .db
        .read(move |c| targets::get(c, &t))
        .await
        .is_ok_and(|r| r.def.lab_from_workspace);
    let res = if per_holder {
        executor::maybe_down_as(state, &l.target, "lease_end", Some(&l.agent)).await
    } else {
        match executor::running_holders(state, &l.target, Some(&l.agent)).await {
            Ok(h) if h.is_empty() => {
                executor::maybe_down_as(state, &l.target, "lease_end", Some(&l.agent)).await
            }
            Ok(_) => executor::detach(state, &l.target, &l.agent, "lease_end").await,
            Err(e) => Err(e),
        }
    };
    if let Err(e) = res {
        warn!(target = %l.target, "cannot bring the target down after its lease: {e}");
    }
}

/// Run [`expire`] at `at` (a stored time). The task holds a weak handle,
/// so it does not keep a stopped daemon alive.
pub fn schedule(state: &Shared, at: &str) {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(at) else {
        return;
    };
    let wait = (t.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .unwrap_or_default()
        + Duration::from_millis(50);
    let weak: Weak<AppState> = Arc::downgrade(state);
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        let Some(state) = weak.upgrade() else { return };
        if let Err(e) = expire(&state).await {
            warn!("lease expiry failed: {e}");
        }
    });
}

/// The sweep: [`expire`] from the daemon start, then each
/// `state.lease_sweep` interval ([`SWEEP`] unless a test sets another).
pub fn spawn_timer(state: &Shared) -> JoinHandle<()> {
    let weak: Weak<AppState> = Arc::downgrade(state);
    tokio::spawn(async move {
        // It wakes at least each second, so a new interval applies soon.
        let mut last: Option<std::time::Instant> = None;
        loop {
            let every = {
                let Some(state) = weak.upgrade() else { return };
                let every = state.lease_sweep.every();
                if last.is_none_or(|l| l.elapsed() >= every) {
                    if let Err(e) = expire(&state).await {
                        warn!("lease expiry failed: {e}");
                    }
                    last = Some(std::time::Instant::now());
                }
                every
            };
            tokio::time::sleep(every.min(Duration::from_secs(1))).await;
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn until_takes_a_time_or_a_duration() {
        assert_eq!(parse_until(None).unwrap(), None);
        assert_eq!(parse_until(Some(" ")).unwrap(), None);
        let in30 = parse_until(Some("30m")).unwrap().unwrap();
        let t = chrono::DateTime::parse_from_rfc3339(&in30).unwrap();
        let d = t.with_timezone(&chrono::Utc) - chrono::Utc::now();
        assert!(d.num_seconds() > 1790 && d.num_seconds() <= 1800, "{d}");
        assert!(in30.ends_with('Z'));
        let later = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        assert!(parse_until(Some(&later)).unwrap().is_some());
        assert!(parse_until(Some("2000-01-01T00:00:00Z")).is_err());
        assert!(parse_until(Some("soon")).is_err());
        assert!(parse_until(Some("0m")).is_err());
    }

    #[test]
    fn the_grant_line_names_the_state() {
        assert_eq!(
            granted_line("vlab", "running", Some("2026-10-25T10:13:44.170Z")),
            "You now have access to target vlab (running) until 2026-10-25T10:13:44.170Z. \
             Run `terrarium targets ls`."
        );
        assert_eq!(
            granted_line("dbox", "stopped", None),
            "You now have access to target dbox (stopped: ask your lead to bring it up) \
             until revoked. Run `terrarium targets ls`."
        );
        assert!(granted_line("h", "unknown", None).contains("target h (unknown) until"));
        assert_eq!(ended_line("vlab"), "Your access to target vlab ended.");
    }

    #[test]
    fn the_display_hint_names_the_loop_and_the_lease() {
        let h = display_hint("win", true);
        assert!(h.contains("terrarium screen win shot"), "{h}");
        assert!(h.contains("`type`"), "{h}");
        assert!(display_hint("win", false).contains("exclusive lease"));
    }
}
