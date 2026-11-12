//! Persistent dependency waits. Configuration and completion share the board transaction.
use super::*;

const MAX_DEPS: usize = 128;
const MAX_GRAPH: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub card: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    All,
    Any,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Action {
    #[serde(default)]
    pub move_to: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub wake: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wait {
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub action: Action,
    #[serde(default)]
    pub met_at: Option<String>,
}

/// Configure a new generation of the wait. Caller-supplied met_at is ignored.
pub fn set(t: &mut Tx<'_>, actor: &Actor, id: &str, wait: &Wait) -> ApiResult<()> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    not_archived(&card)?;
    if !policy::is_admin_of(actor, &card.project) && !owns(actor, &card) {
        return Err(ApiError::forbidden(
            "only the owner or a project admin sets a card wait",
        ));
    }
    if wait.dependencies.is_empty() || wait.dependencies.len() > MAX_DEPS {
        return Err(ApiError::invalid("a card wait needs 1 to 128 dependencies"));
    }
    if wait
        .action
        .comment
        .as_ref()
        .is_some_and(|c| c.trim().is_empty())
    {
        return Err(ApiError::invalid("a wait action comment must not be empty"));
    }
    if let Some(to) = &wait.action.move_to {
        column(t, &card.project, to)?;
    }
    let mut wanted = Vec::new();
    for dep in &wait.dependencies {
        let (project, key) = dep
            .card
            .split_once(':')
            .map_or((None, dep.card.as_str()), |(p, c)| (Some(p), c));
        let key = normalise_id(key);
        let row = find_row(t, &key)?;
        policy::read_project(actor, &row.project)?;
        if project.is_some_and(|p| p != row.project) {
            return Err(ApiError::not_found("no card in that project"));
        }
        not_archived(&row)?;
        if let Some(target) = &dep.target {
            column(t, &row.project, target)?;
        }
        if wanted.iter().any(|(id, _)| id == &key) {
            return Err(ApiError::invalid("duplicate dependency"));
        }
        wanted.push((key, dep.target.clone()));
    }
    check_cycles(
        t,
        &card.id,
        &wanted.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
    )?;
    t.execute("DELETE FROM card_deps WHERE card = ?1", [&card.id])?;
    for (dep, target) in wanted {
        t.execute(
            "INSERT INTO card_deps(card, depends_on, target) VALUES (?1, ?2, ?3)",
            params![card.id, dep, target],
        )?;
    }
    t.execute("INSERT INTO card_waits(card, mode, action, setter, generation) VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(card) DO UPDATE SET mode=excluded.mode, action=excluded.action,
        setter=excluded.setter, generation=excluded.generation, met_at=NULL, acted_at=NULL, broken=0",
        params![card.id, if wait.mode == Mode::All { "all" } else { "any" }, serde_json::to_string(&wait.action)?, actor.address(), ulid::Ulid::new().to_string()])?;
    card_event(t, &card, "edited", json!({"fields": ["wait"]}))?;
    evaluate(t, &card.id, false)
}

/// One visited set and work budget for all roots, also used by legacy depends_on.
pub(super) fn check_cycles(conn: &Connection, card: &str, roots: &[String]) -> ApiResult<()> {
    let mut seen = HashSet::new();
    let mut stack = roots.to_vec();
    let mut statement =
        conn.prepare("SELECT depends_on FROM card_deps WHERE card = ?1 LIMIT 129")?;
    while let Some(next) = stack.pop() {
        if next == card {
            return Err(ApiError::invalid("card wait makes a dependency cycle"));
        }
        if !seen.insert(next.clone()) {
            continue;
        }
        if seen.len() > MAX_GRAPH {
            return Err(ApiError::invalid("dependency graph exceeds 4096 cards"));
        }
        let edges = statement
            .query_map([next], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if edges.len() > MAX_DEPS {
            return Err(ApiError::invalid(
                "dependency graph exceeds 128 edges per card",
            ));
        }
        stack.extend(edges);
    }
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Option<Wait>> {
    let config: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT mode, action, met_at FROM card_waits WHERE card = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((mode, action, met_at)) = config else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT depends_on, target FROM card_deps WHERE card = ?1 ORDER BY depends_on LIMIT 128",
    )?;
    let dependencies = stmt
        .query_map([id], |r| {
            Ok(Dependency {
                card: r.get(0)?,
                target: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(Wait {
        dependencies,
        mode: if mode == "any" { Mode::Any } else { Mode::All },
        action: serde_json::from_str(&action).map_err(|e| ApiError::invalid(e.to_string()))?,
        met_at,
    }))
}

/// Clear configuration and dependencies with exactly the setter's ownership gate.
pub fn clear(t: &mut Tx<'_>, actor: &Actor, id: &str) -> ApiResult<()> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    not_archived(&card)?;
    if !policy::is_admin_of(actor, &card.project) && !owns(actor, &card) {
        return Err(ApiError::forbidden(
            "only the owner or a project admin clears a card wait",
        ));
    }
    t.execute("DELETE FROM card_deps WHERE card=?1", [&card.id])?;
    t.execute("DELETE FROM card_waits WHERE card=?1", [&card.id])?;
    card_event(t, &card, "edited", json!({"fields": ["wait"]}))
}

/// Authorized presentation. Completion is latched, dependency conditions are live.
pub fn view(conn: &Connection, actor: &Actor, id: &str) -> ApiResult<serde_json::Value> {
    let card = find_row(conn, id)?;
    crate::store::threads::read_card_project(conn, actor, &card.project, None)?;
    let Some(wait) = get(conn, &card.id)? else {
        return Ok(json!(null));
    };
    let mut result = json!(wait);
    let (generation, setter): (String, String) = conn.query_row(
        "SELECT generation, setter FROM card_waits WHERE card=?1",
        [&card.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    result["generation"] = json!(generation);
    result["wake_recipient"] = json!(wake_recipient(conn, &card, &setter)?.key());
    let mut dependencies = Vec::new();
    for dep in &wait.dependencies {
        let row = find_row(conn, &dep.card)?;
        let met: bool = conn.query_row(
            "SELECT c.archived = 0 AND CASE WHEN ?2 IS NULL
             THEN coalesce(k.role,k.name) IN ('accepted','done')
             ELSE k.name=?2 OR coalesce(k.role=?2,0) END
             FROM cards c JOIN board_columns k ON k.id=c.column_id WHERE c.id=?1",
            params![dep.card, dep.target],
            |r| r.get(0),
        )?;
        dependencies.push(if policy::read_project(actor, &row.project).is_err() {
            json!({"card": null, "hidden": true, "met": met})
        } else {
            json!({"card": dep.card, "target": dep.target, "met": met, "hidden": false})
        });
    }
    result["dependencies"] = json!(dependencies);
    Ok(result)
}

fn wake_recipient(conn: &Connection, row: &CardRow, setter: &str) -> ApiResult<Address> {
    let target = [row.assignee.as_deref(), Some(setter)]
        .into_iter()
        .flatten()
        .filter(|a| *a != "operator")
        .find_map(|a| mail::resolve(conn, a, None).ok());
    match target {
        Some(to) => Ok(to),
        None => Ok(mail::inbox_recipient(conn, &row.project)?.unwrap_or(Address::Operator)),
    }
}

pub(super) fn evaluate(t: &mut Tx<'_>, id: &str, broken: bool) -> ApiResult<()> {
    let open: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM card_waits WHERE card=?1 AND met_at IS NULL)",
        [id],
        |r| r.get(0),
    )?;
    if !open || (!broken && !unmet(t, id)?.is_empty()) {
        return Ok(());
    }
    t.execute(
        "UPDATE card_waits SET met_at=?2, broken=?3 WHERE card=?1 AND met_at IS NULL",
        params![id, now(), broken],
    )?;
    let row = find_row(t, id)?;
    let generation: String = t.query_row(
        "SELECT generation FROM card_waits WHERE card=?1",
        [id],
        |r| r.get(0),
    )?;
    card_event(
        t,
        &row,
        "wait_met",
        json!({"broken": broken, "wait_generation": generation}),
    )?;
    if !crate::store::projects::rules_flow(t, &row.project)? {
        let actor = crate::rules_engine::rule_actor(&row.project, "card-wait-met");
        act(t, &actor, id, &generation)?;
    }
    Ok(())
}

pub(super) fn on_dependency_changed(t: &mut Tx<'_>, id: &str, broken: bool) -> ApiResult<()> {
    // Keyset pages use the reverse index. Do not load the whole board.
    let mut after = String::new();
    loop {
        let mut stmt = t.prepare(
            "SELECT d.card FROM card_deps d JOIN card_waits w ON w.card=d.card
            WHERE d.depends_on=?1 AND d.card>?2 AND w.met_at IS NULL ORDER BY d.card LIMIT 128",
        )?;
        let page = stmt
            .query_map(params![id, after], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        if page.is_empty() {
            break;
        }
        for card in &page {
            evaluate(t, card, broken)?;
        }
        after = page.last().cloned().unwrap_or_default();
    }
    Ok(())
}

/// Pack rule action. The latch survives a restart and duplicate event delivery.
pub fn act(t: &mut Tx<'_>, actor: &Actor, id: &str, generation: &str) -> ApiResult<bool> {
    let row = find_row(t, id)?;
    policy::write_project(actor, &row.project)?;
    // Keep completion and the dependency change outside the action savepoint.
    // Tx::savepoint also discards staged events/files/audits on failure.
    match t.savepoint(|t| act_inner(t, actor, id, generation)) {
        Ok(acted) => Ok(acted),
        Err(error) => {
            t.execute("UPDATE card_waits SET acted_at=?3 WHERE card=?1 AND generation=?2 AND met_at IS NOT NULL", params![id, generation, now()])?;
            t.audit(
                "card.wait_action_failed",
                id,
                Some(&row.project),
                json!({"reason": error.to_string(), "wait_generation": generation}),
            )?;
            Ok(true)
        }
    }
}

fn act_inner(t: &mut Tx<'_>, actor: &Actor, id: &str, generation: &str) -> ApiResult<bool> {
    let config: Option<(String, String, bool)> = t
        .query_row(
            "SELECT action, setter, broken FROM card_waits
        WHERE card=?1 AND generation=?2 AND met_at IS NOT NULL AND acted_at IS NULL",
            params![id, generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((action, setter, broken)) = config else {
        return Ok(false);
    };
    let action: Action =
        serde_json::from_str(&action).map_err(|e| ApiError::invalid(e.to_string()))?;
    let row = find_row(t, id)?;
    policy::write_project(actor, &row.project)?;
    t.execute(
        "UPDATE card_waits SET acted_at=?2 WHERE card=?1",
        params![id, now()],
    )?;
    let mut reason = if broken {
        "wait broken: a dependency was archived".to_owned()
    } else {
        "wait met".to_owned()
    };
    let mut refused = false;
    if !broken {
        if let Some(to) = action.move_to {
            // check_move runs before writes, so a refusal leaves the move untouched.
            let checked = column(t, &row.project, &to).and_then(|destination| {
                check_freeze(t, &row.project)?;
                check_move(t, actor, &row, &destination, action.comment.as_deref())?;
                Ok(destination)
            });
            match checked {
                Ok(destination) => apply_move(
                    t,
                    actor,
                    &row,
                    &destination,
                    None,
                    action.comment.as_deref(),
                )?,
                Err(e) => {
                    reason = format!("wait met, move refused: {e}");
                    refused = true;
                    t.audit(
                        "card.wait_action_failed",
                        id,
                        Some(&row.project),
                        json!({"reason": reason, "wait_generation": generation}),
                    )?;
                }
            }
        } else if let Some(comment) = action.comment {
            add_comment(t, actor, id, &comment)?;
        }
    }
    if action.wake || broken || refused {
        let to = wake_recipient(t, &row, &setter)?;
        mail::send(
            t,
            Outgoing {
                from: &Address::System,
                to: &to,
                subject: &format!("{}: {reason}", row.id),
                body: &format!(
                    "Card {}: {reason}. Run `terrarium card show {}`.",
                    row.id, row.id
                ),
                thread: None,
                in_reply_to: None,
            },
        )?;
    }
    Ok(true)
}

/// Calendar scopes are capped at MAX_EVENTS. Read only the two relevant scopes
/// through the project index, including recurring events and their exceptions.
fn check_freeze(conn: &Connection, project: &str) -> ApiResult<()> {
    use crate::store::calendar;
    let at = chrono::Utc::now();
    for scope in [Some(project), None] {
        let mut stmt = conn.prepare(
            "SELECT id FROM calendar_events WHERE project IS ?1 ORDER BY start_at, id LIMIT ?2",
        )?;
        let ids = stmt
            .query_map(params![scope, calendar::MAX_EVENTS + 1], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > calendar::MAX_EVENTS {
            return Err(ApiError::conflict(
                "calendar freeze check exceeds event limit",
            ));
        }
        for id in ids {
            let event = calendar::get(conn, &id)?;
            if event.kind != "freeze" {
                continue;
            }
            let (occurrences, cut) =
                calendar::expand(&[event], at, at + chrono::Duration::milliseconds(1));
            if cut
                || occurrences.iter().any(|o| {
                    calendar::parse_utc(&o.start).is_some_and(|start| start <= at)
                        && calendar::parse_utc(&o.end).is_some_and(|end| at < end)
                })
            {
                return Err(ApiError::conflict(format!(
                    "calendar freeze `{id}` prevents automatic move"
                )));
            }
        }
    }
    Ok(())
}
