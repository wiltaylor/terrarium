//! Target displays, the daemon side (PLAN §19b, docs/api.md §17.10).
//!
//! One API call is one display operation: the daemon checks the rights,
//! the input rule (an exclusive lease), and the rate limit, writes the
//! audit row of an input operation, builds the `display` frame for the
//! target's host agent (with the password of a `vnc` or `rdp` target, and
//! only there), and waits for the answer. A screenshot comes back as the
//! session's `stdout` bytes. The daemon stores no screenshot.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use terrarium_core::config::{TargetDef, TargetKind};
use terrarium_core::display::{
    DisplayOp, DisplayProtocol, INPUT_OPS_PER_SEC, MAX_SCREENSHOT_BYTES,
};
use terrarium_host::protocol::{DaemonFrame, DisplayRequest, DisplayTarget};

use crate::actor::Actor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult, Code};
use crate::executor::{End, container_name, pump_host, resolved_def};
use crate::policy;
use crate::secrets::Secret;
use crate::store::leases::{self, LeaseRow};
use crate::store::targets::{self, TargetRow};

/// The limit of one operation on the host agent.
const OP_TIMEOUT_SECS: u64 = 60;
/// `type` on vmlab sends one process for each character.
const TYPE_TIMEOUT_SECS: u64 = 180;

/// True when the target has a screen.
pub fn has_display(def: &TargetDef) -> bool {
    match def.kind {
        TargetKind::Vnc | TargetKind::Rdp | TargetKind::Desktop => true,
        TargetKind::Docker => def.display.is_some(),
        TargetKind::Vmlab => def.machine.is_some(),
        TargetKind::Host | TargetKind::Ssh | TargetKind::Exedev => false,
    }
}

/// The input rate limit: at most [`INPUT_OPS_PER_SEC`] input operations
/// on one target in one second (a fixed window for each target).
#[derive(Debug, Default)]
pub struct Limiter {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
    #[cfg(feature = "fake-supervisor")]
    test_time: Mutex<Option<Instant>>,
}

impl Limiter {
    /// Drive only the display rate window in integration tests, without
    /// changing the clocks used by the host transport or daemon timers.
    #[cfg(feature = "fake-supervisor")]
    pub fn set_test_time(&self, now: Instant) {
        *self.test_time.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
    }

    pub fn check(&self, target: &str) -> ApiResult<()> {
        let now = Instant::now();
        #[cfg(feature = "fake-supervisor")]
        let now = self
            .test_time
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(now);
        self.check_at(target, now)
    }

    fn check_at(&self, target: &str, now: Instant) -> ApiResult<()> {
        let mut w = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        // Drop the old windows, so the map does not grow.
        if w.len() > 1024 {
            w.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(1));
        }
        let entry = w.entry(target.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(1) {
            *entry = (now, 0);
        }
        if entry.1 >= INPUT_OPS_PER_SEC {
            return Err(ApiError::new(
                Code::RateLimited,
                format!(
                    "more than {INPUT_OPS_PER_SEC} input operations in one second on target \
                     `{target}`; wait, and send fewer actions"
                ),
            ));
        }
        entry.1 += 1;
        Ok(())
    }
}

/// What the actor may do on the screen of a target.
#[derive(Debug, Clone)]
pub struct Rights {
    /// It may read the screen (the use right, or an admin).
    pub read: bool,
    /// The refusal of input, when input is not allowed now.
    pub input: Result<(), ApiError>,
}

/// An admin of the target: a sysadmin, or the project_admin of the
/// target's project.
fn admin_of(actor: &Actor, def: &TargetDef) -> bool {
    match actor {
        Actor::Operator => true,
        Actor::Agent(a) => {
            a.is_sysadmin() || def.project.as_deref().is_some_and(|p| a.is_admin_of(p))
        }
    }
}

fn lease_held(l: &LeaseRow, target: &str) -> ApiError {
    ApiError::new(
        Code::LeaseHeld,
        format!(
            "{} holds an exclusive lease on target `{target}` until {}; only it may send input",
            l.holder(),
            l.until()
        ),
    )
    .with_details(
        json!({"holder": l.agent, "holder_name": l.agent_name, "expires_at": l.expires_at}),
    )
}

/// The rights of `actor` on the screen of `row` (docs/api.md §17.10,
/// "Rights").
pub async fn rights(state: &Shared, actor: &Actor, row: &TargetRow) -> ApiResult<Rights> {
    let Actor::Agent(a) = actor else {
        return Ok(Rights {
            read: true,
            input: Ok(()),
        });
    };
    let (name, me) = (row.def.name.clone(), a.id.clone());
    let (standing, active) = state
        .db
        .read(move |c| {
            Ok((
                leases::standing(c, &name, &me)?,
                leases::active_on(c, &name)?,
            ))
        })
        .await?;
    let name = &row.def.name;
    let usable = policy::use_target(actor, name, row.def.project.as_deref(), standing);
    let admin = admin_of(actor, &row.def);
    if usable.is_err() && !admin {
        return Ok(Rights {
            read: false,
            input: Err(usable
                .err()
                .unwrap_or_else(|| ApiError::forbidden("no access"))),
        });
    }
    let input = if let Some(l) = active.iter().find(|l| l.exclusive && l.agent != a.id) {
        Err(lease_held(l, name))
    } else if admin || active.iter().any(|l| l.exclusive && l.agent == a.id) {
        Ok(())
    } else {
        Err(ApiError::conflict(format!(
            "input on target `{name}` needs an exclusive lease, so two agents do not move one \
             mouse: ask your lead for `terrarium admin target grant {name} {} --exclusive`",
            a.name
        )))
    };
    Ok(Rights { read: true, input })
}

/// The body fields of each operation, besides `agent`.
fn op_fields(op: &str) -> &'static [&'static str] {
    match op {
        "screenshot" => &["region", "scale"],
        "move" => &["x", "y"],
        "click" => &["x", "y", "button", "double"],
        "drag" => &["x1", "y1", "x2", "y2"],
        "scroll" => &["x", "y", "dx", "dy"],
        "keys" => &["chord"],
        "type" => &["text"],
        "ocr" => &["region"],
        "find" => &["image_base64", "threshold", "region"],
        "record" => &["action"],
        _ => &[],
    }
}

/// The operation of `POST .../display/{op}` and its `agent`. An unknown
/// operation is `404`; an unknown field or a bad value is `422`.
pub fn parse_op(op: &str, body: Value) -> ApiResult<(DisplayOp, Option<String>)> {
    if !terrarium_core::display::OP_NAMES.contains(&op) {
        return Err(ApiError::not_found(format!(
            "no display operation `{op}`; use one of {}",
            terrarium_core::display::OP_NAMES.join(", ")
        )));
    }
    let Value::Object(mut m) = body else {
        return Err(ApiError::bad_request("the body is a JSON object"));
    };
    let agent = match m.remove("agent") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(_) => return Err(ApiError::invalid("`agent` is an agent name or ID")),
    };
    m.remove("op");
    let allowed = op_fields(op);
    if let Some(k) = m.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(ApiError::invalid(format!(
            "`{op}` takes no field `{k}`; its fields: {}",
            if allowed.is_empty() {
                "none".to_owned()
            } else {
                allowed.join(", ")
            }
        )));
    }
    m.insert("op".into(), json!(op));
    let parsed: DisplayOp = serde_json::from_value(Value::Object(m))
        .map_err(|e| ApiError::invalid(format!("bad `{op}` body: {e}")))?;
    Ok((parsed, agent))
}

/// The answer of one operation.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// The `result` of the host agent, with `target`.
    pub value: Value,
    /// The PNG of a screenshot.
    pub png: Option<Vec<u8>>,
}

/// The password and the default user name of a `vnc` or `rdp` target.
fn secret(state: &Shared, def: &TargetDef) -> ApiResult<(Option<String>, Option<String>)> {
    let Some(name) = &def.credential else {
        return Ok((def.username.clone(), None));
    };
    // The secret of a project target: the project's first, then the
    // global one (PLAN §16b.2).
    match crate::secrets::load_now(state, crate::store::targets::owner(def), name)? {
        Secret::Login { username, password } => Ok((
            def.username.clone().or(username),
            Some(password.to_string()),
        )),
        other => Err(ApiError::invalid(format!(
            "secret `{name}` is a {}; a {} target needs a `login` secret",
            other.kind_label(),
            targets::kind_str(def.kind)
        ))),
    }
}

/// The screen of a target, for the `display` frame.
pub fn display_target(state: &Shared, def: &TargetDef) -> ApiResult<DisplayTarget> {
    Ok(match def.kind {
        TargetKind::Vmlab => DisplayTarget::Vmlab {
            lab: def.lab.clone().ok_or_else(|| {
                ApiError::invalid(format!("vmlab target `{}` has no `lab`", def.name))
            })?,
            machine: def.machine.clone().ok_or_else(|| {
                ApiError::invalid(format!(
                    "vmlab target `{}` has no `machine`; the display is the screen of one machine",
                    def.name
                ))
            })?,
        },
        TargetKind::Docker => DisplayTarget::Docker {
            container: container_name(def),
            protocol: def.display.ok_or_else(|| no_display(def))?,
            display: def.display_name.clone(),
        },
        TargetKind::Desktop => DisplayTarget::Desktop {
            protocol: def.display.unwrap_or(DisplayProtocol::Auto),
            display: def.display_name.clone(),
        },
        TargetKind::Vnc => {
            let (username, password) = secret(state, def)?;
            DisplayTarget::Vnc {
                address: targets::display_address(def)?,
                username,
                password,
            }
        }
        TargetKind::Rdp => {
            let (username, password) = secret(state, def)?;
            DisplayTarget::Rdp {
                address: targets::display_address(def)?,
                username,
                domain: def.domain.clone(),
                password,
            }
        }
        TargetKind::Host | TargetKind::Ssh | TargetKind::Exedev => {
            return Err(no_display(def));
        }
    })
}

fn no_display(def: &TargetDef) -> ApiError {
    ApiError::invalid(format!(
        "target `{}` ({}) has no display: a display is on vnc, rdp, desktop, and vmlab \
         targets, and on a docker target with `display` (x11, wayland, or auto)",
        def.name,
        targets::kind_str(def.kind)
    ))
}

/// Map the end of a `display` session to an API error.
fn failed(end: &End, stderr: &str) -> ApiError {
    match end {
        End::Error { code, message } => {
            let e = match code.as_str() {
                "refused" => ApiError::forbidden(message.clone()),
                "busy" => ApiError::new(Code::RateLimited, message.clone()),
                _ => ApiError::invalid(message.clone()),
            };
            e.with_details(json!({"code": code}))
        }
        other => ApiError::invalid(format!("the display operation failed: {stderr}"))
            .with_details(json!({"code": "failed", "end": other})),
    }
}

/// Run one display operation for `actor` on target `name`.
pub async fn run(
    state: &Shared,
    actor: &Actor,
    name: &str,
    op: DisplayOp,
    agent: Option<String>,
) -> ApiResult<Answer> {
    let n = name.to_owned();
    let row = state.db.read(move |c| targets::get(c, &n)).await?;
    let r = rights(state, actor, &row).await?;
    if !r.read {
        return Err(r
            .input
            .err()
            .unwrap_or_else(|| ApiError::forbidden(format!("no access to target `{name}`"))));
    }
    if !has_display(&row.def) {
        return Err(no_display(&row.def));
    }
    op.check().map_err(ApiError::invalid)?;
    // Computer use is work: the idle clock of the agent starts again
    // (`crate::lifecycle`).
    if let Some(me) = actor.agent() {
        state.lifecycle.touch(&me.id);
    }
    if op.is_input() {
        r.input?;
        state.executor.display_limiter.check(name)?;
    }
    let def = resolved_def(state, &row, Some(actor), agent.as_deref()).await?;
    let host = def
        .host
        .clone()
        .ok_or_else(|| ApiError::invalid(format!("target `{name}` names no host agent")))?;
    if !state.executor.hosts.is_online(&host) {
        return Err(ApiError::conflict(format!(
            "the host agent `{host}` of target `{name}` is offline"
        )));
    }
    if def.kind == TargetKind::Desktop
        && !state
            .executor
            .hosts
            .capabilities(&host)
            .is_some_and(|c| c.display.enabled)
    {
        return Err(ApiError::forbidden(format!(
            "the desktop display of host `{host}` is off: its host agent's own config needs \
             [display] enabled = true (the daemon cannot turn it on)"
        )));
    }
    let target = display_target(state, &def)?;
    if op.is_input() {
        let mut detail = op.audit_detail();
        detail["backend"] = json!(target.backend());
        if let Some(a) = &agent {
            detail["agent"] = json!(a);
        }
        let (t, p, label) = (row.def.name.clone(), row.def.project.clone(), actor.label());
        state
            .db
            .write(label, move |tx| {
                tx.audit("target.display", &t, p.as_deref(), detail)
            })
            .await?;
    }
    let timeout = if matches!(op, DisplayOp::Type { .. }) {
        TYPE_TIMEOUT_SECS
    } else {
        OP_TIMEOUT_SECS
    };
    let op_name = op.name();
    let hs = state.executor.hosts.open(&host, move |session| {
        DaemonFrame::Display(Box::new(DisplayRequest {
            session,
            target,
            request: op,
            timeout_secs: Some(timeout),
        }))
    })?;
    let c = pump_host(hs).collect(MAX_SCREENSHOT_BYTES + 1).await;
    let end = c.end.clone().unwrap_or(End::Error {
        code: "lost".into(),
        message: "the display session ended without an end".into(),
    });
    if !end.success() {
        let stderr = String::from_utf8_lossy(&c.stderr).trim().to_owned();
        return Err(failed(&end, &stderr));
    }
    let mut value = c.result.unwrap_or_else(|| json!({"op": op_name}));
    value["target"] = json!(name);
    let png = if op_name == "screenshot" {
        if c.stdout.len() > MAX_SCREENSHOT_BYTES {
            return Err(ApiError::invalid(format!(
                "the screenshot is over {MAX_SCREENSHOT_BYTES} bytes; ask for a region or a smaller scale"
            )));
        }
        Some(c.stdout)
    } else {
        None
    };
    Ok(Answer { value, png })
}

/// `info`, with the input rule of the caller.
pub async fn info(
    state: &Shared,
    actor: &Actor,
    name: &str,
    agent: Option<String>,
) -> ApiResult<Value> {
    let mut a = run(state, actor, name, DisplayOp::Info, agent).await?;
    let n = name.to_owned();
    let row = state.db.read(move |c| targets::get(c, &n)).await?;
    let r = rights(state, actor, &row).await?;
    a.value["kind"] = json!(targets::kind_str(row.def.kind));
    a.value["input"] = match r.input {
        Ok(()) => json!({"allowed": true}),
        Err(e) => json!({"allowed": false, "reason": e.message}),
    };
    Ok(a.value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_limiter_allows_20_a_second_on_each_target() {
        let l = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..INPUT_OPS_PER_SEC {
            l.check_at("a", t0).unwrap();
        }
        let e = l.check_at("a", t0).unwrap_err();
        assert_eq!(e.code, Code::RateLimited);
        l.check_at("b", t0).unwrap();
        l.check_at("a", t0 + Duration::from_millis(1001)).unwrap();
    }

    #[test]
    fn display_kinds() {
        let def = |t: &str| -> TargetDef { toml::from_str(t).unwrap() };
        assert!(has_display(&def("kind = \"vnc\"")));
        assert!(has_display(&def("kind = \"desktop\"")));
        assert!(!has_display(&def("kind = \"docker\"\nimage = \"a\"")));
        assert!(has_display(&def(
            "kind = \"docker\"\nimage = \"a\"\ndisplay = \"x11\""
        )));
        assert!(has_display(&def(
            "kind = \"vmlab\"\nlab = \"/l\"\nmachine = \"m\""
        )));
        assert!(!has_display(&def("kind = \"vmlab\"\nlab = \"/l\"")));
        assert!(!has_display(&def("kind = \"ssh\"\naddress = \"h\"")));
    }
}
