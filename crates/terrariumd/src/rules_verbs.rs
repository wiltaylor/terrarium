//! The verbs of the board rules (TER-162 D6, TER-169, docs/api.md §19.9).
//!
//! The engine ([`crate::rules_engine`]) runs each verb in its own write,
//! as the rule actor (`rule:<project>/<rule>`, `project_admin` of its
//! project only). Each verb goes through the store functions that the API
//! uses, so the store's checks hold for it as for a lead: the project
//! gate, the board invariants (WIP limits, dependencies, the blocked
//! comment), and the job queue rules (a merge needs `project_admin`, one
//! open job per card).
//!
//! - **Held cards.** No verb acts on a card that the GitHub author gate
//!   holds (TER-85): the run's card, or a card of the event's job for a
//!   `job.*` verb. Only the operator releases it.
//! - **No-ops.** A verb whose precondition does not hold changes nothing
//!   and reports `changed: false` (a card not in `from`, a label it has, a
//!   job that is not red, a rerun that runs already). A run of only no-ops
//!   ends as `noop`.
//! - **Once per job.** `job.verdict` comes once for each card of a batch,
//!   so a rule on it runs once per card. `job.rerun`, `job.split` and
//!   `job.fix` act once for a job; the later runs are no-ops.
//! - **After the commit.** What cannot run in a database write runs after
//!   the action's commit ([`Effect`]): the queue tick, the kill of a
//!   cancelled running job, the owner's queue and the wake of an unparked
//!   agent, and the close of a bound agent (`agent.end`). A daemon stop
//!   between the commit and the effect loses the effect: the queue ticks
//!   every few seconds, the spawn timer runs the queued unparks and ends
//!   the agent of a finished card.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_core::rules_file::{self, Action};
use terrarium_core::turn_tag::{self, SystemKind, TurnSource};
use tracing::warn;

use crate::actor::Actor;
use crate::api::Shared;
use crate::card_dispatcher::{self, Pick, Templates};
use crate::api::jobs as api_jobs;
use crate::db::Tx;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::rules_engine::{Done, RunCtx, lookup};
use crate::store::accepted_heads;
use crate::store::board::{self, Card, CardPatch, NewCard, NewLink};
use crate::store::jobs::{self, BatchCard, Job, JobKind, JobState, NewJob};
use crate::store::mail::{self, Address};
use crate::store::schedules;
use crate::store::spawns::{self, Bound};

/// The arguments whose text an agent reads: their placeholders are filled
/// as quoted data (one line, in backticks). `card.prepend` is the
/// exception: its text goes into the card body as it is, so a failure
/// block keeps its lines (the card body is data to an agent already).
pub const QUOTED_ARGS: &[&str] = &[
    "text", "comment", "subject", "body", "turn", "reason", "result",
];

/// Whether the placeholders of argument `key` of `verb` are quoted.
pub fn quoted(verb: &str, key: &str) -> bool {
    QUOTED_ARGS.contains(&key) && verb != "card.prepend"
}

/// The label of the fix card of a red merge job (`job.fix`).
pub fn fix_label(job: &str) -> String {
    format!("merge-fix:{job}")
}

/// What runs after the commit of an action.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Start a queue tick (a job was queued, cancelled, or split).
    Kick,
    /// Kill the run of a cancelled job that was running.
    Kill(String),
    /// Record the reviewed commit of the cards that came into the merge
    /// queue (TER-553).
    RecordAccepted,
    /// Run the queued unparks of an owner.
    RetryOwner(String),
    /// Wake an unparked agent.
    Wake(String),
    /// Close the bound agent of an object.
    End {
        bound: Bound,
        outcome: &'static str,
        result: String,
    },
    /// Start new bound agents for cards (`card.assign` with a template,
    /// TER-1115): up to `limit` cards of `pick`, as the agent `owner`.
    Dispatch {
        project: String,
        owner: String,
        pick: Pick,
        limit: i64,
    },
}

/// Run the effects of an action, in order, after its commit.
pub async fn apply(state: &Shared, effects: Vec<Effect>) {
    for e in effects {
        match e {
            Effect::Kick => crate::jobs::kick(state),
            Effect::Kill(id) => crate::jobs::kill(&id),
            Effect::RecordAccepted => crate::jobs::record_accepted_soon(state),
            Effect::RetryOwner(owner) => crate::bound::retry_owner(state, &owner).await,
            Effect::Wake(id) => {
                if let Ok(a) = id.parse::<AgentId>() {
                    crate::lifecycle::wake_soon(state, a, "rule unpark");
                }
            }
            Effect::End {
                bound,
                outcome,
                result,
            } => {
                if let Err(e) = crate::bound::end(state, &bound, outcome, result, true).await {
                    warn!(bound = %bound.text(), "a rule cannot close the bound agent: {e}");
                }
            }
            Effect::Dispatch {
                project,
                owner,
                pick,
                limit,
            } => dispatch(state, &project, &owner, &pick, limit).await,
        }
    }
}

/// The spawns of a `card.assign` with a template, after the commit. The
/// owner and the selection are read again; each spawn rechecks them in its
/// transaction. A card that fails gets the dispatcher's cooldown, and the
/// next card is tried.
async fn dispatch(state: &Shared, project: &str, owner: &str, pick: &Pick, limit: i64) {
    for _ in 0..limit.clamp(1, rules_file::MAX_ASSIGN_LIMIT) {
        let (p, o) = (project.to_owned(), owner.to_owned());
        let who = state
            .db
            .read(move |c| card_dispatcher::named_owner(c, &p, &o, "card.assign owner"))
            .await;
        let who = match who {
            Ok(Some(who)) => who,
            Ok(None) => return,
            Err(e) => {
                warn!(%project, %owner, "card.assign cannot start an agent: {e}");
                return;
            }
        };
        let source = card_dispatcher::Owner::Named(owner.to_owned());
        match card_dispatcher::run(state, project, who, pick.clone(), source).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::debug!(%project, error = %e.message, "card.assign spawn deferred");
                if pick.card.is_some() {
                    return;
                }
            }
        }
    }
}

/// The arguments of one action, filled from the run's state.
struct Args<'a> {
    action: &'a Action,
    ctx: &'a RunCtx,
}

impl Args<'_> {
    /// The argument as written, without templates (a column, a word).
    fn raw(&self, key: &str) -> Option<&str> {
        self.action.args.get(key).and_then(Value::as_str)
    }

    /// The text of `key` with its placeholders filled.
    fn text(&self, key: &str) -> Option<String> {
        let q = quoted(&self.action.verb, key);
        self.raw(key)
            .map(|s| rules_file::render(s, &|n| lookup(self.ctx, n), q))
    }

    /// The texts of a list argument, filled, trimmed, without the empty
    /// ones. An item with a placeholder that has no value is left out
    /// (`by:{{job.by_agent}}` without a worker), not kept half filled.
    fn list(&self, key: &str) -> Vec<String> {
        let items: Vec<&str> = match self.action.args.get(key) {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(s)) => vec![s.as_str()],
            _ => Vec::new(),
        };
        items
            .into_iter()
            .filter_map(|s| {
                let (v, empty) = rules_file::render_with(s, &|n| lookup(self.ctx, n), false);
                let v = v.trim().to_owned();
                (!empty && !v.is_empty()).then_some(v)
            })
            .collect()
    }

    /// The column names of a column or column list argument.
    fn columns(&self, key: &str) -> ApiResult<Vec<String>> {
        let refs: Vec<&str> = match self.action.args.get(key) {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(s)) => vec![s.as_str()],
            _ => Vec::new(),
        };
        refs.into_iter().map(|r| column_name(self.ctx, r)).collect()
    }

    fn count(&self, key: &str) -> Option<i64> {
        self.action.args.get(key).and_then(Value::as_i64)
    }
}

fn column_name(ctx: &RunCtx, reference: &str) -> ApiResult<String> {
    rules_file::resolve_column(&ctx.columns, reference)
        .map(|c| c.name.clone())
        .ok_or_else(|| {
            ApiError::invalid(format!(
                "column `{reference}` is not on the board of project `{}`",
                ctx.project
            ))
        })
}

fn changed(detail: Value) -> Done {
    Done {
        changed: true,
        detail,
    }
}

fn noop(detail: Value) -> Done {
    Done {
        changed: false,
        detail,
    }
}

/// Are all placeholders of a raw text `group.*` ones (`{{group.failed_step}}`)?
/// A text without a placeholder answers true; the caller checks that first.
fn only_group_placeholders(raw: &str) -> bool {
    raw.split("{{").skip(1).all(|rest| {
        rest.find("}}")
            .is_some_and(|close| rest[..close].trim().starts_with("group."))
    })
}

fn the_card<'a>(ctx: &'a RunCtx, verb: &str) -> ApiResult<&'a Card> {
    ctx.card
        .as_ref()
        .ok_or_else(|| ApiError::invalid(format!("{verb} needs an event with a card")))
}

fn the_job<'a>(ctx: &'a RunCtx, verb: &str) -> ApiResult<&'a Job> {
    ctx.job.as_ref().ok_or_else(|| {
        ApiError::invalid(format!(
            "{verb} needs a `job` or `job.verdict` event whose job is in the store"
        ))
    })
}

/// The GitHub author gate (TER-85): no verb but the move mails acts on a
/// held card (TER-536). The
/// run's card, and for a `job.*` verb each card of the event's job.
fn refuse_held(conn: &Connection, ctx: &RunCtx, verb: &str) -> ApiResult<()> {
    let mut cards: Vec<String> = ctx.card.iter().map(|c| c.id.clone()).collect();
    if verb.starts_with("job.")
        && let Some(j) = &ctx.job
    {
        for c in j.cards() {
            if !cards.contains(&c.card) {
                cards.push(c.card);
            }
        }
    }
    for c in cards {
        match board::is_held(conn, &c) {
            Ok(true) => {
                return Err(ApiError::forbidden(format!(
                    "{c} is held by the GitHub author gate: no rule verb acts on it until the \
                     operator releases it"
                )));
            }
            Ok(false) => {}
            // A card that is gone holds nothing.
            Err(e) if e.code.status().is_client_error() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Run `action` as `actor` in the run's write. Returns what it did and
/// the effects to run after the commit.
pub fn perform(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    action: &Action,
) -> ApiResult<(Done, Vec<Effect>)> {
    let verb = action.verb.as_str();
    // The move mails only notify: a held card moved to blocked or done
    // gets its mail in rules flow as in code flow (TER-536).
    if !matches!(
        verb,
        "card.blocked_mail" | "card.done_mail" | "card.wait_action"
    ) {
        refuse_held(t, ctx, verb)?;
    }
    let a = Args { action, ctx };
    let mut effects = Vec::new();
    let done = match verb {
        "version.cut" => {
            let key = crate::store::version_groups::cut(
                t,
                actor,
                &ctx.project,
                &ctx.rule,
                a.action
                    .args
                    .get("min_cards")
                    .and_then(Value::as_i64)
                    .unwrap_or(6),
                a.action
                    .args
                    .get("max_wait_minutes")
                    .and_then(Value::as_i64)
                    .unwrap_or(240),
            )?;
            Done {
                changed: key.is_some(),
                detail: json!({"key":key}),
            }
        }
        "group.open" | "group.add" | "group.remove" | "group.status" | "group.close" => {
            group_verb(t, actor, ctx, &a)?
        }
        "card.move" => card_move(t, actor, ctx, &a, &mut effects)?,
        "card.wait_action" => {
            if !ctx
                .event
                .as_ref()
                .is_some_and(|e| e.kind == "card" && e.data["change"] == "wait_met")
            {
                return Err(ApiError::invalid(
                    "card.wait_action needs a `card` event with change wait_met",
                ));
            }
            let card = the_card(ctx, verb)?;
            let generation = ctx
                .event
                .as_ref()
                .and_then(|e| e.data["wait_generation"].as_str())
                .unwrap_or_default();
            let acted = board::card_waits::act(t, actor, &card.id, generation)?;
            if acted {
                let moved = board::get_card(t, &card.id)?;
                if accepted_heads::is_queue(&moved.column, moved.column_role.as_deref()) {
                    effects.push(Effect::RecordAccepted);
                }
            }
            Done {
                changed: acted,
                detail: json!({"card": card.id}),
            }
        }
        "card.label" => card_label(t, actor, ctx, &a)?,
        "card.comment" => {
            let card = the_card(ctx, verb)?;
            let body = a.text("text").unwrap_or_default();
            board::add_comment(t, actor, &card.id, &body)?;
            changed(json!({"card": card.id, "text": body}))
        }
        "card.assign" => card_assign(t, actor, ctx, &a, &mut effects)?,
        "card.prepend" => card_prepend(t, actor, ctx, &a)?,
        "mail" => send_mail(t, actor, ctx, &a)?,
        "card.blocked_mail" | "card.done_mail" => move_mail(t, actor, ctx, verb)?,
        "job.queue" => job_queue(t, actor, ctx, &a, &mut effects)?,
        "job.cancel" => job_cancel(t, actor, ctx, &mut effects)?,
        "job.rerun" => job_rerun(t, actor, ctx, &a, &mut effects)?,
        "job.split" => job_split(t, actor, ctx, &a, &mut effects)?,
        "job.fix" => job_fix(t, actor, ctx, &a)?,
        "agent.park" | "agent.unpark" | "agent.end" => agent_verb(t, actor, ctx, &a, &mut effects)?,
        other => return Err(ApiError::invalid(format!("unknown verb `{other}`"))),
    };
    Ok((done, effects))
}

// ---------------------------------------------------------- card verbs

fn group_verb(t: &mut Tx<'_>, actor: &Actor, ctx: &RunCtx, a: &Args<'_>) -> ApiResult<Done> {
    use crate::store::groups;
    policy::write_project(actor, &ctx.project)?;
    let key = a.text("key").unwrap_or_default();
    let verb = a.action.verb.as_str();
    let did = if verb == "group.open" {
        groups::open(
            t,
            groups::Open {
                project: &ctx.project,
                rule: &ctx.rule,
                key: &key,
                title: &a.text("title").unwrap_or_default(),
                column: &column_name(ctx, a.raw("column").unwrap_or_default())?,
                done_when: &column_name(ctx, a.raw("done_when").unwrap_or_default())?,
                eta: a.text("eta").as_deref(),
                close_after: a.text("close_after").as_deref(),
            },
        )?
    } else {
        let Some(g) = groups::by_key(t, &ctx.project, &key)? else {
            // A job queued before the pack was installed has no header.
            // Its completion must still run the ordinary delivery actions.
            if a.raw("key") == Some("{{job.group_key}}") && ctx.job.is_some() {
                return Ok(noop(
                    json!({"key": key, "reason": "job has no merge group"}),
                ));
            }
            return Err(ApiError::invalid(format!("no group with key `{key}`")));
        };
        match verb {
            "group.add" => {
                let card = the_card(ctx, verb)?;
                let added = groups::add(t, &g, &card.id)?;
                // Per-card rule lanes may run in either order. Keep the
                // merge group's membership in the queued batch order.
                if added
                    && a.raw("key") == Some("{{job.group_key}}")
                    && let Some(job) = &ctx.job
                {
                    for (position, member) in job.cards().iter().enumerate() {
                        t.execute("UPDATE group_members SET position = ?3 WHERE group_card = ?1 AND card = ?2",
                            params![g.id, member.card, position as i64])?;
                    }
                }
                added
            }
            "group.remove" => groups::remove(t, &g, &the_card(ctx, verb)?.id)?,
            "group.status" => match a.text("step") {
                // The group of the key is open (`g`), but the run's card
                // left it, so a `{{group.*}}` step placeholder has no
                // value (`{{group.failed_step}}` of the pack, on
                // `merge-group-failed`: a red split re-groups the batch
                // while the per-card lanes of the old verdict still mark
                // the old group). There is no step to mark then: skip, as
                // a key without a group skips. A closed group noops in
                // `groups::step`, which stays first, and any other step
                // that renders empty (a literal one, or the rule's own
                // placeholder) stays invalid.
                Some(step) if step.trim().is_empty() && g.closed_at.is_none() => {
                    let raw = a.raw("step").unwrap_or_default();
                    let cardless = ctx.card.as_ref().is_none_or(|c| c.group.is_none());
                    if raw.contains("{{") && (cardless || only_group_placeholders(raw)) {
                        return Ok(noop(json!({
                            "key": key,
                            "reason": if cardless {
                                "the step placeholder has no value \
                                 (the card has no group)"
                            } else {
                                "the step placeholder has no value"
                            },
                        })));
                    }
                    return Err(ApiError::invalid("group step name must not be empty"));
                }
                Some(step) => groups::step(
                    t,
                    &g,
                    &step,
                    a.raw("state"),
                    a.text("text").as_deref(),
                    a.text("eta").as_deref(),
                )?,
                None => {
                    if a.raw("state").is_some() {
                        return Err(ApiError::invalid("group_status state requires step"));
                    }
                    groups::status(t, &g, a.text("text").as_deref(), a.text("eta").as_deref())?
                }
            },
            "group.close"
                if a.action.args.get("all_done").and_then(Value::as_bool) == Some(true)
                    && (g.total == 0 || g.done != g.total) =>
            {
                false
            }
            "group.close" => groups::close(
                t,
                &g,
                &a.text("comment")
                    .unwrap_or_else(|| format!("Closed by rule {}", ctx.rule)),
            )?,
            _ => unreachable!(),
        }
    };
    Ok(Done {
        changed: did,
        detail: json!({"key": key}),
    })
}

fn card_move(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    a: &Args<'_>,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    let card = the_card(ctx, "card.move")?;
    let to = column_name(ctx, a.raw("to").unwrap_or_default())?;
    let from = a.columns("from")?;
    if !from.is_empty() && !from.contains(&card.column) {
        return Ok(noop(json!({
            "card": card.id, "column": card.column, "to": to,
            "reason": format!("{} is in {}, not in {}", card.id, card.column, from.join(", ")),
        })));
    }
    if card.column == to {
        return Ok(noop(json!({
            "card": card.id, "column": card.column,
            "reason": format!("{} is in {to} already", card.id),
        })));
    }
    let position = (a.raw("position") == Some("top")).then_some(0);
    let comment = a.text("comment");
    let moved = board::move_card(t, actor, &card.id, &to, position, comment.as_deref())?;
    if crate::store::accepted_heads::is_queue(&moved.column, moved.column_role.as_deref()) {
        effects.push(Effect::RecordAccepted);
    }
    Ok(changed(
        json!({"card": card.id, "from": card.column, "to": to}),
    ))
}

/// `card.label`: `remove` takes globs (`by:*`); a label of `add` stays.
fn card_label(t: &mut Tx<'_>, actor: &Actor, ctx: &RunCtx, a: &Args<'_>) -> ApiResult<Done> {
    let card = the_card(ctx, "card.label")?;
    let add: Vec<String> = a
        .list("add")
        .into_iter()
        .filter(|l| !card.labels.contains(l))
        .collect();
    let want = a.list("add");
    let patterns = a.list("remove");
    let remove: Vec<String> = card
        .labels
        .iter()
        .filter(|l| {
            !want.contains(l)
                && patterns
                    .iter()
                    .any(|p| p == *l || (p.contains('*') && terrarium_core::schedule::glob(p, l)))
        })
        .cloned()
        .collect();
    if add.is_empty() && remove.is_empty() {
        return Ok(noop(json!({
            "card": card.id, "labels": card.labels,
            "reason": "the labels are as asked already",
        })));
    }
    let patch = CardPatch {
        add_labels: add.clone(),
        remove_labels: remove.clone(),
        ..CardPatch::default()
    };
    board::update_card(t, actor, &card.id, patch)?;
    Ok(changed(
        json!({"card": card.id, "added": add, "removed": remove}),
    ))
}

fn card_assign(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    a: &Args<'_>,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    if a.action.args.get("who").is_none_or(Value::is_null) {
        return assign_new(t, actor, ctx, a, effects);
    }
    let card = the_card(ctx, "card.assign")?;
    let raw = a.raw("who").unwrap_or_default().trim();
    let who = a.text("who").unwrap_or_default().trim().to_owned();
    let who = if raw == "none" {
        None
    } else if who.is_empty() {
        // A placeholder without a value (no `by:` label) does not
        // unassign: only `none` does.
        return Err(ApiError::invalid(format!(
            "card.assign: `{raw}` is empty for {}",
            card.id
        )));
    } else {
        Some(who)
    };
    let after = board::assign_card(t, actor, &card.id, who.as_deref())?;
    let detail = json!({"card": card.id, "assignee": after.assignee_name.clone().or(after.assignee.clone()),
                        "previous": card.assignee_name.clone().or(card.assignee.clone())});
    Ok(if after.assignee == card.assignee {
        noop(detail)
    } else {
        changed(detail)
    })
}

/// `card.assign` with `template` or `templates` (TER-1115, D6): a new bound
/// agent from a template claims the card, as the `card_dispatcher` setting
/// does. The owner's spawn rights and caps are used. This write selects
/// the card and reports it; the spawn runs after the commit
/// ([`Effect::Dispatch`]). No room, no eligible card, a paused owner, or an
/// owner at a spawn cap is a no-op with the reason.
fn assign_new(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    a: &Args<'_>,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    policy::admin_project(
        actor,
        &ctx.project,
        "starting an agent for a card of the project",
    )?;
    let owner = a.text("owner").unwrap_or_default().trim().to_owned();
    if owner.is_empty() {
        return Err(ApiError::invalid("card.assign needs `owner` for a new agent"));
    }
    let templates = match a.text("template").map(|t| t.trim().to_owned()) {
        Some(one) if !one.is_empty() => Templates::One(one),
        Some(_) => return Err(ApiError::invalid("card.assign: `template` is empty")),
        None => {
            let mut map = Vec::new();
            for e in a.list("templates") {
                let (label, template) = rules_file::assign_template_entry(&e).ok_or_else(|| {
                    ApiError::invalid(format!(
                        "card.assign.templates entry `{e}` is not `<label>=<template>`"
                    ))
                })?;
                map.push((label.to_owned(), template.to_owned()));
            }
            if map.is_empty() {
                return Err(ApiError::invalid(
                    "card.assign needs `who`, `template` or `templates`",
                ));
            }
            Templates::ByLabel(map)
        }
    };
    let room = a.raw("room").map(|r| column_name(ctx, r)).transpose()?;
    let (from, card, limit) = match a.raw("from") {
        Some(r) => (
            column_name(ctx, r)?,
            None,
            a.count("limit")
                .unwrap_or(1)
                .clamp(1, rules_file::MAX_ASSIGN_LIMIT),
        ),
        None => {
            let c = the_card(ctx, "card.assign")?;
            (c.column.clone(), Some(c.id.clone()), 1)
        }
    };
    let Some(who) = card_dispatcher::named_owner(t, &ctx.project, &owner, "card.assign owner")?
    else {
        return Ok(noop(json!({"owner": owner, "reason": "the owner is paused"})));
    };
    if let Some(reason) = card_dispatcher::project_limit(t, &who)? {
        return Ok(noop(json!({"owner": owner, "reason": reason})));
    }
    let pick = Pick {
        from,
        room,
        templates,
        card,
    };
    match card_dispatcher::select(t, &ctx.project, &pick)? {
        Err(reason) => Ok(noop(
            json!({"from": pick.from, "card": pick.card, "reason": reason}),
        )),
        Ok((first, template)) => {
            let detail = json!({"card": first, "template": template, "owner": owner,
                                "from": pick.from, "limit": limit,
                                "spawn": "after the commit"});
            effects.push(Effect::Dispatch {
                project: ctx.project.clone(),
                owner,
                pick,
                limit,
            });
            Ok(changed(detail))
        }
    }
}

fn card_prepend(t: &mut Tx<'_>, actor: &Actor, ctx: &RunCtx, a: &Args<'_>) -> ApiResult<Done> {
    let card = the_card(ctx, "card.prepend")?;
    let text = a.text("text").unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(noop(
            json!({"card": card.id, "reason": "the text is empty"}),
        ));
    }
    if card.body.starts_with(&text) {
        return Ok(noop(
            json!({"card": card.id, "reason": "the body starts with the text already"}),
        ));
    }
    let body = if card.body.is_empty() {
        text.clone()
    } else if text.ends_with('\n') {
        format!("{text}{}", card.body)
    } else {
        format!("{text}\n\n{}", card.body)
    };
    let patch = CardPatch {
        body: Some(body),
        ..CardPatch::default()
    };
    board::update_card(t, actor, &card.id, patch)?;
    Ok(changed(
        json!({"card": card.id, "chars": text.chars().count()}),
    ))
}

// ------------------------------------------------------------------ mail

/// Mail from the project inbox, as the mail of a schedule. A recipient
/// whose placeholder has no value is left out; none at all is a no-op.
fn send_mail(t: &mut Tx<'_>, actor: &Actor, ctx: &RunCtx, a: &Args<'_>) -> ApiResult<Done> {
    policy::admin_project(
        actor,
        &ctx.project,
        &format!("sending mail as project:{}", ctx.project),
    )?;
    let to = a.list("to");
    if to.is_empty() {
        return Ok(noop(json!({"reason": "no recipient"})));
    }
    let subject = a.text("subject").unwrap_or_default();
    let body = a
        .text("body")
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| subject.clone());
    let from = Address::Project(ctx.project.clone());
    let mut sent = Vec::new();
    let mut recipients = Vec::new();
    for who in &to {
        if who == "@leads" {
            recipients.extend(crate::owner::leads(t, &ctx.project, "")?);
        } else {
            recipients.push(mail::resolve(t, who, Some(&ctx.project))?);
        }
    }
    recipients.sort_by_key(Address::key);
    recipients.dedup();
    for addr in recipients {
        if matches!(addr, Address::System) {
            return Err(ApiError::invalid("mail cannot go to `system`"));
        }
        let m = crate::api::mail::send_checked(t, actor, &from, &addr, &subject, &body, None)?;
        sent.push(json!({"id": m.id, "to": addr.key()}));
    }
    Ok(changed(json!({"messages": sent, "subject": subject})))
}

/// The mails of a card move that the coded flow sends in `apply_move`
/// (TER-162 D10): `card.blocked_mail` mails the blocked notice from
/// `system` to the operator and the card's creator, not to the mover;
/// `card.done_mail` mails the done notice to the participants of each
/// linked thread. The texts are the coded flow's. Only on the `moved`
/// event of a move to a blocked column, or to `done`: else a no-op.
fn move_mail(t: &mut Tx<'_>, actor: &Actor, ctx: &RunCtx, verb: &str) -> ApiResult<Done> {
    let card = the_card(ctx, verb)?;
    let event = ctx
        .event
        .as_ref()
        .filter(|e| e.kind == "card")
        .ok_or_else(|| ApiError::invalid(format!("{verb} needs a `card` event")))?;
    // With `board_flow = code` the daemon's move sends the mail: the verb
    // would send it twice (TER-536).
    if crate::jobs::code_flow(t, &card.project)? {
        return Ok(noop(
            json!({"card": card.id, "reason": "board_flow = code sends the move mail"}),
        ));
    }
    let data = &event.data;
    let blocked = verb == "card.blocked_mail";
    let fits = data["change"] == "moved"
        && if blocked {
            data["to_role"] == board::BLOCKED
        } else {
            data["to"] == board::DONE
        };
    if !fits {
        return Ok(noop(
            json!({"card": card.id, "reason": "not a move to the column of the mail"}),
        ));
    }
    policy::admin_project(
        actor,
        &card.project,
        &format!("sending the move mail of {}", card.id),
    )?;
    let comment = data["comment"].as_str();
    if blocked {
        // The mover: `author` of the event; an older event has only its
        // actor (`agent:<id>`, `operator`, `system`, ...).
        let author = match data["author"].as_str() {
            Some(a) => a.to_owned(),
            None => match event.actor.as_deref() {
                Some("operator") => "operator".to_owned(),
                Some(a) => a.strip_prefix("agent:").unwrap_or("system").to_owned(),
                None => "system".to_owned(),
            },
        };
        let column = data["to"].as_str().unwrap_or(&card.column);
        board::mail_blocked(t, &card.id, column, &author, comment.unwrap_or_default())?;
    } else {
        board::mail_done(t, &card.id, comment)?;
    }
    Ok(changed(json!({"card": card.id})))
}

// ------------------------------------------------------------- job verbs

/// The worker of a card's job: its `by:<agent>` label, else its agent
/// assignee.
fn worker_of(card: &Card) -> Option<String> {
    crate::jobs::by_agent_of(&card.labels).or_else(|| {
        card.assignee
            .as_ref()
            .filter(|a| a.as_str() != "operator")
            .and(card.assignee_name.clone())
    })
}

fn open_job(conn: &Connection, card: &str) -> ApiResult<Option<Job>> {
    jobs::open_of_card(conn, card)
}

fn job_queue(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    a: &Args<'_>,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    let kind = a.raw("kind").unwrap_or_default();
    if kind == "batch" {
        return queue_batch(t, actor, ctx, a, effects);
    }
    let card = the_card(ctx, "job.queue")?;
    let kind = match kind {
        "validate" => {
            api_jobs::may_validate(actor, card)?;
            JobKind::Validate
        }
        "merge" => {
            policy::admin_project(actor, &card.project, "queueing a merge onto next")?;
            api_jobs::not_held(card)?;
            JobKind::Merge
        }
        other => {
            return Err(ApiError::invalid(format!(
                "job.queue: unknown kind `{other}`"
            )));
        }
    };
    if card.archived {
        return Err(ApiError::conflict(format!("card {} is archived", card.id)));
    }
    let def = jobs::delivery(t, &card.project)?;
    api_jobs::enabled(&def, &card.project)?;
    let repo = api_jobs::repo_of(t, card, None)?;
    if kind == JobKind::Merge && !def.next_repos.contains(&repo) {
        return Err(ApiError::conflict(format!(
            "repo `{repo}` has no `next` branch in the queue config: add it to `next_repos`"
        )));
    }
    let branch = api_jobs::branch_of(card, None)?;
    // A merge takes the reviewed commit that the accept recorded, never
    // the head at run time (TER-553). A validate tests the head.
    let commit = if kind == JobKind::Merge {
        match accepted_heads::commit_for(t, &card.id, &branch)? {
            Some(c) => Some(c),
            None => {
                let why = accepted_heads::missing_reason(t, &card.id, &branch)?;
                return Ok(noop(json!({"card": card.id, "reason": why})));
            }
        }
    } else {
        None
    };
    if let Some(open) = open_job(t, &card.id)? {
        return Ok(noop(json!({
            "card": card.id, "job": open.id,
            "reason": format!("{} has a {} job {} that is {}", card.id, open.kind.as_str(),
                              open.id, open.state.as_str()),
        })));
    }
    let job = jobs::create(
        t,
        NewJob {
            project: card.project.clone(),
            card: card.id.clone(),
            kind,
            repo,
            branch,
            commit,
            by_agent: worker_of(card),
            batch: Vec::new(),
            planned_secs: None,
        },
    )?;
    if kind == JobKind::Merge && crate::jobs::code_flow(t, &job.project)? {
        crate::jobs::mark_queued(t, &job)?;
    }
    effects.push(Effect::Kick);
    Ok(changed(
        json!({"job": job.id, "kind": job.kind, "card": card.id}),
    ))
}

/// The cards of a batch of column `from` (TER-172).
#[derive(Debug, Clone, Default)]
pub struct BatchPick {
    /// The cards that the batch takes: board order, at most the limit.
    /// Routing fields only, not rendered card details (`board::selection_cards`).
    pub cards: Vec<Card>,
    /// The cards of the column that wait for a batch (not held, no open
    /// job), apart from the limit.
    pub waiting: usize,
    /// When the first of those came into the column (its newest move,
    /// else its creation).
    pub first_since: Option<DateTime<Utc>>,
    /// The cards of the column that the batch leaves out, with the reason.
    pub left_out: Vec<Value>,
    /// The repo of the batch: the repo of the first card that waits. A
    /// card of another repo is left out and waits for a later batch.
    pub repo: Option<String>,
    /// The batch is one card with the [`RELEASE_BLOCKER`] label (TER-607):
    /// it goes at once, alone, before the other cards.
    pub release_blocker: bool,
}

/// The label of a release-blocker fix (TER-607): the merge queue takes the
/// first card with it alone and first, as a batch of one that goes at
/// once. After it is on `next`, `card merge --onto release/X.Y.Z` merges
/// the same commit into the frozen version.
pub const RELEASE_BLOCKER: &str = "release-blocker";

/// The cards of column `from` of `project` that a batch takes, in board
/// order, at most `limit` (`None`: all). A card that is held, has an
/// open job, or has no usable repo is left out. A batch is one repo: the repo of the first card
/// that waits. A card of another repo is left out and does not count as
/// waiting, so it does not block the queue; it goes in a later batch
/// (TER-227). There is no limit of the daemon's own: the rule sets it
/// (TER-162 D9).
///
/// A card that could go and has the [`RELEASE_BLOCKER`] label is the
/// whole batch (TER-607): the first such card in board order, of any
/// repo, alone, and the others wait for the next batch.
pub fn pick_batch(
    conn: &Connection,
    project: &str,
    from: &str,
    limit: Option<usize>,
) -> ApiResult<BatchPick> {
    let cards = board::selection_cards(conn, project, from)?;
    let mut out = BatchPick::default();
    // The cards that could go, with their repo, in board order.
    let mut ready: Vec<(&Card, String)> = Vec::new();
    for c in &cards {
        if api_jobs::not_held(c).is_err() {
            out.left_out.push(json!({"card": c.id, "reason": "held"}));
            continue;
        }
        if let Some(j) = open_job(conn, &c.id)? {
            out.left_out
                .push(json!({"card": c.id, "reason": format!("open job {}", j.id)}));
            continue;
        }
        // A card goes in a batch only with its reviewed commit, the head
        // that the review accept recorded (TER-553); until then it waits,
        // and does not count as waiting.
        let branch = match api_jobs::branch_of(c, None) {
            Ok(b) => b,
            Err(e) if e.code == Code::Invalid => {
                out.left_out
                    .push(json!({"card": c.id, "reason": e.message}));
                continue;
            }
            Err(e) => return Err(e),
        };
        if accepted_heads::commit_for(conn, &c.id, &branch)?.is_none() {
            let why = accepted_heads::missing_reason(conn, &c.id, &branch)?;
            out.left_out.push(json!({"card": c.id, "reason": why}));
            continue;
        }
        // A card without a usable repo (none in a project of several
        // repos, or one that is not the project's) is left out, so it
        // does not stop the batch or the timers (TER-227 review).
        let r = match api_jobs::repo_of(conn, c, None) {
            Ok(r) => r,
            Err(e) if e.code == Code::Invalid => {
                out.left_out
                    .push(json!({"card": c.id, "reason": e.message}));
                continue;
            }
            Err(e) => return Err(e),
        };
        ready.push((c, r));
    }
    if let Some(i) = ready
        .iter()
        .position(|(c, _)| c.labels.iter().any(|l| l == RELEASE_BLOCKER))
    {
        let (c, r) = ready.remove(i);
        for (o, _) in ready {
            out.left_out.push(json!({
                "card": o.id,
                "reason": format!("the release-blocker card {} goes first, alone", c.id),
            }));
        }
        out.waiting = 1;
        out.first_since = since_of(conn, c)?;
        out.repo = Some(r);
        out.cards.push(c.clone());
        out.release_blocker = true;
        return Ok(out);
    }
    for (c, r) in ready {
        match &out.repo {
            Some(have) if *have != r => {
                out.left_out.push(json!({
                    "card": c.id,
                    "reason": format!("repo `{r}`, the batch is repo `{have}`"),
                }));
                continue;
            }
            Some(_) => {}
            None => out.repo = Some(r),
        }
        out.waiting += 1;
        if let Some(t) = since_of(conn, c)? {
            out.first_since = Some(out.first_since.map_or(t, |f| f.min(t)));
        }
        if limit.is_none_or(|l| out.cards.len() < l) {
            out.cards.push(c.clone());
        }
    }
    Ok(out)
}

/// When `c` came into its column: its newest move, else its creation.
fn since_of(conn: &Connection, c: &Card) -> ApiResult<Option<DateTime<Utc>>> {
    let since = board::last_moved_at(conn, &c.id)?.unwrap_or_else(|| c.created_at.clone());
    Ok(DateTime::parse_from_rfc3339(&since)
        .ok()
        .map(|t| t.with_timezone(&Utc)))
}

/// When a batch with `min_cards` and `max_wait_minutes` goes: `Ok` now,
/// `Err((reason, due))` when it waits, with the time it goes by the
/// wait (`None`: only a card more makes it go). A release-blocker batch
/// goes now (TER-607).
pub fn batch_goes(
    pick: &BatchPick,
    min_cards: Option<i64>,
    max_wait_minutes: Option<i64>,
    now: DateTime<Utc>,
) -> Result<(), (String, Option<DateTime<Utc>>)> {
    if pick.release_blocker {
        return Ok(());
    }
    let Some(min) = min_cards.and_then(|n| usize::try_from(n).ok()) else {
        return Ok(());
    };
    if pick.waiting >= min {
        return Ok(());
    }
    let due = max_wait_minutes
        .zip(pick.first_since)
        .map(|(m, first)| first + chrono::Duration::minutes(m));
    match due {
        Some(d) if d <= now => Ok(()),
        Some(d) => Err((
            format!(
                "{} of {min} cards wait; the batch goes at {} ({} minutes after the first one)",
                pick.waiting,
                schedules::stamp(d),
                max_wait_minutes.unwrap_or_default()
            ),
            Some(d),
        )),
        None => Err((format!("{} of {min} cards wait", pick.waiting), None)),
    }
}

/// One merge job for the cards of column `from` in board order, at most
/// `limit`. A card that is held or has an open job is left out. With
/// `min_cards` the job waits for that many cards, or (with
/// `max_wait_minutes`) until the first card waited that long; until then
/// the verb is a no-op that says when the batch goes. While a merge job
/// of the project is queued or running (the merge lane is busy), the verb
/// is a no-op too (TER-644).
fn queue_batch(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    a: &Args<'_>,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    let project = ctx.project.clone();
    policy::admin_project(actor, &project, "queueing a merge onto next")?;
    let from = column_name(ctx, a.raw("from").unwrap_or_default())?;
    let limit = a.count("limit").and_then(|n| usize::try_from(n).ok());
    let def = jobs::delivery(t, &project)?;
    api_jobs::enabled(&def, &project)?;
    let pick = pick_batch(t, &project, &from, limit)?;
    let left_out = pick.left_out.clone();
    if pick.cards.is_empty() {
        return Ok(noop(json!({
            "column": from, "left_out": left_out,
            "reason": format!("no card to merge in {from}"),
        })));
    }
    // One merge at a time per project: while a merge job holds the lane,
    // the cards wait and the batch grows. The timer checks again when
    // the lane is free (TER-644).
    if let Some(job) = jobs::open_merge(t, &project)? {
        return Ok(noop(json!({
            "column": from, "waiting": pick.waiting, "left_out": left_out,
            "lane": job,
            "reason": format!("the merge lane is busy: job {job}; the batch waits for its end"),
        })));
    }
    if let Err((reason, due)) = batch_goes(
        &pick,
        a.count("min_cards"),
        a.count("max_wait_minutes"),
        Utc::now(),
    ) {
        return Ok(noop(json!({
            "column": from, "waiting": pick.waiting, "left_out": left_out,
            "due": due.map(schedules::stamp), "reason": reason,
        })));
    }
    let mut batch = Vec::new();
    for c in &pick.cards {
        let branch = api_jobs::branch_of(c, None)?;
        let commit = accepted_heads::commit_for(t, &c.id, &branch)?;
        batch.push(BatchCard {
            card: c.id.clone(),
            branch,
            by_agent: crate::jobs::by_agent_of(&c.labels),
            commit,
            outcome: None,
            note: None,
        });
    }
    let repo = pick.repo.clone().unwrap_or_default();
    if !def.next_repos.contains(&repo) {
        return Err(ApiError::conflict(format!(
            "repo `{repo}` has no `next` branch in the queue config: add it to `next_repos`"
        )));
    }
    let first = batch[0].clone();
    let names: Vec<String> = batch.iter().map(|c| c.card.clone()).collect();
    let job = jobs::create(
        t,
        NewJob {
            project: project.clone(),
            card: first.card.clone(),
            kind: JobKind::Merge,
            repo,
            branch: first.branch.clone(),
            commit: first.commit.clone(),
            by_agent: first.by_agent.clone(),
            batch: if batch.len() > 1 { batch } else { Vec::new() },
            planned_secs: None,
        },
    )?;
    if crate::jobs::code_flow(t, &project)? {
        crate::jobs::mark_queued(t, &job)?;
    }
    effects.push(Effect::Kick);
    Ok(changed(
        json!({"job": job.id, "kind": "merge", "cards": names, "left_out": left_out}),
    ))
}

/// Cancel the event's job, else the open job of the run's card.
fn job_cancel(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    policy::admin_project(actor, &ctx.project, "cancelling another agent's job")?;
    let job = match (&ctx.job, &ctx.card) {
        (Some(j), _) => Some(j.clone()),
        (None, Some(c)) => open_job(t, &c.id)?,
        (None, None) => None,
    };
    let Some(job) = job.filter(|j| j.state.is_open()) else {
        return Ok(noop(json!({"reason": "no open job"})));
    };
    let was_running = job.state == JobState::Running;
    let j = jobs::cancel(t, &job.id)?;
    crate::jobs::emit_verdicts(t, &j)?;
    // As the API cancel: with `board_flow = code` the labels go now; with
    // `rules` the rule pack takes them off on the verdict (TER-170).
    if crate::jobs::code_flow(t, &j.project)? {
        crate::jobs::clear_job_labels(t, &j)?;
    }
    if was_running {
        effects.push(Effect::Kill(j.id.clone()));
    }
    effects.push(Effect::Kick);
    Ok(changed(json!({"job": j.id, "was": job.state})))
}

/// The cards of a job that did not conflict, and whose branch did not
/// move after its review (TER-553).
fn merged(j: &Job) -> Vec<BatchCard> {
    j.cards()
        .into_iter()
        .filter(|c| !crate::jobs::left_merge(c.outcome.as_deref()))
        .collect()
}

/// Why no new job runs for `cards`: a card that has an open job already
/// (the coded flow split the batch, or someone queued it again). The verb
/// is then a no-op, not a failure.
fn busy(conn: &Connection, cards: &[BatchCard]) -> ApiResult<Option<String>> {
    for c in cards {
        if let Some(open) = open_job(conn, &c.card)? {
            return Ok(Some(format!(
                "card {} has a {} job {} that is {} already",
                c.card,
                open.kind.as_str(),
                open.id,
                open.state.as_str()
            )));
        }
    }
    Ok(None)
}

/// Is `card` a fix card (it has a `merge-fix:` label)?
fn is_fix_card(conn: &Connection, card: &str) -> ApiResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM cards c, json_each(c.labels) l
              WHERE c.id = ?1 AND l.value LIKE 'merge-fix:%'",
            [card],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The fix cards of this group from earlier jobs (TER-170, MEM-84): the
/// cards with a `merge-fix:` label of another job whose linked cards, fix
/// cards left out, equal the cards of `j` or an explicit split ancestor. So
/// the fix cards of the group count for its rerun (the fix card first in
/// it) and for its split halves, and a fix card of another group that
/// shared a card with this one does not. Done and archived ones count:
/// each was an attempt.
fn earlier_fixes(conn: &Connection, j: &Job) -> ApiResult<i64> {
    let mut ancestors = Vec::new();
    let mut parent = j.parent.clone();
    while let Some(id) = parent {
        let job = jobs::get(conn, &id)?;
        let mut cards = Vec::new();
        for c in job.cards() {
            if !is_fix_card(conn, &c.card)? {
                cards.push(c.card);
            }
        }
        ancestors.push(cards);
        parent = job.parent;
    }
    let mut group: Vec<String> = Vec::new();
    for c in j.cards() {
        if !is_fix_card(conn, &c.card)? {
            group.push(c.card);
        }
    }
    let own = fix_label(&j.id);
    let candidates: Vec<String> = {
        let mut st = conn.prepare(
            "SELECT DISTINCT l.card FROM card_links l JOIN cards c ON c.id = l.card
              WHERE l.kind = 'card' AND l.ref IN (SELECT value FROM json_each(?1))
                AND c.project = ?2
                AND EXISTS (SELECT 1 FROM json_each(c.labels)
                             WHERE value LIKE 'merge-fix:%' AND value <> ?3)",
        )?;
        st.query_map(
            params![serde_json::to_string(&group)?, j.project, own],
            |r| r.get(0),
        )?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut n = 0;
    for fix in candidates {
        let linked: Vec<String> = {
            let mut st =
                conn.prepare("SELECT ref FROM card_links WHERE card = ?1 AND kind = 'card'")?;
            st.query_map([&fix], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let linked = linked
            .into_iter()
            .filter_map(|c| match is_fix_card(conn, &c) {
                Ok(false) => Some(Ok(c)),
                Ok(true) => None,
                Err(e) => Some(Err(e)),
            })
            .collect::<ApiResult<Vec<_>>>()?;
        if std::iter::once(&group)
            .chain(&ancestors)
            .any(|cards| cards.len() == linked.len() && cards.iter().all(|c| linked.contains(c)))
        {
            n += 1;
        }
    }
    Ok(n)
}

/// Queue the event's failed job once more (not a conflict), while its
/// attempt is under `max_attempts`. A batch runs again with the cards
/// that did not conflict.
fn job_rerun(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    a: &Args<'_>,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    let j = the_job(ctx, "job.rerun")?;
    policy::admin_project(actor, &j.project, "running a job again")?;
    let max = a.count("max_attempts").unwrap_or(2);
    let why = if j.state != JobState::Failed {
        Some(format!("job {} is {}", j.id, j.state.as_str()))
    } else if crate::jobs::left_merge(j.outcome.as_deref()) {
        Some(format!(
            "job {} is a {}: it never runs again",
            j.id,
            j.outcome.as_deref().unwrap_or_default()
        ))
    } else if j.attempt >= max {
        Some(format!("job {} is attempt {} of {max}", j.id, j.attempt))
    } else {
        None
    };
    if let Some(why) = why {
        return Ok(noop(json!({"job": j.id, "reason": why})));
    }
    let cards = merged(j);
    if cards.is_empty() {
        return Ok(noop(
            json!({"job": j.id, "reason": "every card of the job conflicted"}),
        ));
    }
    if let Some(next) = jobs::rerun_of(t, j)? {
        return Ok(noop(json!({
            "job": j.id, "next": next,
            "reason": format!("job {next} runs job {} again already", j.id),
        })));
    }
    if let Some(why) = busy(t, &cards)? {
        return Ok(noop(json!({"job": j.id, "reason": why})));
    }
    let batch = (!j.batch.is_empty()).then_some(cards.as_slice());
    let next = jobs::rerun(t, j, batch)?;
    effects.push(Effect::Kick);
    Ok(changed(
        json!({"job": j.id, "next": next.id, "attempt": next.attempt}),
    ))
}

/// Split the event's red batch (two or more cards that merged) in halves.
/// `after_fixes`: only when the group had that many fix cards of earlier
/// jobs.
fn job_split(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    a: &Args<'_>,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    let j = the_job(ctx, "job.split")?;
    policy::admin_project(actor, &j.project, "splitting a merge batch")?;
    if j.state != JobState::Failed || j.outcome.as_deref() != Some("red") {
        return Ok(noop(json!({
            "job": j.id,
            "reason": format!("job {} is {} ({}), not red", j.id, j.state.as_str(),
                              j.outcome.as_deref().unwrap_or("none")),
        })));
    }
    let cards = merged(j);
    if cards.len() < 2 {
        return Ok(noop(json!({
            "job": j.id,
            "reason": format!("job {} has {} merged card(s); a split needs two or more",
                              j.id, cards.len()),
        })));
    }
    let child: Option<String> = t
        .query_row(
            "SELECT id FROM jobs WHERE parent = ?1 ORDER BY split_path LIMIT 1",
            [&j.id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(c) = child {
        return Ok(noop(json!({
            "job": j.id, "half": c,
            "reason": format!("job {} is split already", j.id),
        })));
    }
    if let Some(after) = a.count("after_fixes") {
        let had = earlier_fixes(t, j)?;
        if had < after {
            return Ok(noop(json!({
                "job": j.id,
                "reason": format!("the group had {had} fix card(s); it splits after {after}"),
            })));
        }
    }
    if let Some(why) = busy(t, &cards)? {
        return Ok(noop(json!({"job": j.id, "reason": why})));
    }
    let (left, right) = jobs::split(t, j, &cards)?;
    effects.push(Effect::Kick);
    let names = |x: &Job| x.cards().into_iter().map(|c| c.card).collect::<Vec<_>>();
    Ok(changed(json!({
        "job": j.id,
        "left": {"job": left.id, "cards": names(&left)},
        "right": {"job": right.id, "cards": names(&right)},
    })))
}

/// Open one fix card for the event's failed merge job (MEM-84): a red
/// merge group gets a fix before a split.
fn job_fix(t: &mut Tx<'_>, actor: &Actor, ctx: &RunCtx, a: &Args<'_>) -> ApiResult<Done> {
    let j = the_job(ctx, "job.fix")?;
    policy::admin_project(actor, &j.project, "opening a fix card for a merge job")?;
    // Only a red merge (its tests failed): a conflict, an error (the job
    // could not run), or a cut run is not for a fix card (TER-169 review).
    if j.kind != JobKind::Merge
        || j.state != JobState::Failed
        || j.outcome.as_deref() != Some("red")
    {
        return Ok(noop(json!({
            "job": j.id,
            "reason": format!("job {} is a {} job that is {} ({}), not a red merge", j.id,
                              j.kind.as_str(), j.state.as_str(),
                              j.outcome.as_deref().unwrap_or("none")),
        })));
    }
    if let Some(max) = a.count("max") {
        let had = earlier_fixes(t, j)?;
        if had >= max {
            return Ok(noop(json!({
                "job": j.id,
                "reason": format!("the group had {had} fix card(s), the most is {max}"),
            })));
        }
    }
    let label = fix_label(&j.id);
    if let Some(open) = board::find_open_by_label(t, &j.project, &label)? {
        join_fix_group(t, ctx, &open)?;
        return Ok(noop(json!({
            "job": j.id, "card": open,
            "reason": format!("{open} is the fix card of job {} already", j.id),
        })));
    }
    let column = match a.raw("to") {
        Some(r) => Some(column_name(ctx, r)?),
        None => rules_file::resolve_column(&ctx.columns, "@ready").map(|c| c.name.clone()),
    };
    let cards: Vec<String> = j.cards().into_iter().map(|c| c.card).collect();
    let mut labels = vec![label.clone()];
    for l in a.list("labels") {
        if !labels.contains(&l) {
            labels.push(l);
        }
    }
    let body = format!(
        "The merge job {} of repo `{}` failed (outcome {}, attempt {}). The cards of the group, in merge order: {}.\n\n\
         Fix the failure on a branch from `next` with the cards of the group merged, run the failing tests, \
         and queue the group again with this card first.\n\n{}",
        j.id,
        j.repo,
        j.outcome.as_deref().unwrap_or("-"),
        j.attempt,
        cards.join(" "),
        crate::jobs::failure_block(j)
    );
    let card = board::create_card(
        t,
        actor,
        &j.project,
        NewCard {
            title: Some(format!(
                "Fix the red merge group of job {} ({} cards)",
                j.id,
                cards.len()
            )),
            body: Some(body),
            labels,
            repo: Some(j.repo.clone()),
            ..NewCard::default()
        },
    )?;
    if let Some(col) = &column
        && *col != card.column
    {
        board::move_card(t, actor, &card.id, col, None, None)?;
    }
    for c in &cards {
        let link = NewLink {
            kind: "card".into(),
            reference: c.clone(),
        };
        match board::add_link(t, actor, &card.id, &link) {
            Ok(_) => {}
            Err(e) if e.code.status().is_client_error() => {}
            Err(e) => return Err(e),
        }
    }
    let at = board::get_card(t, &card.id)?.column;
    join_fix_group(t, ctx, &card.id)?;
    Ok(changed(
        json!({"job": j.id, "card": card.id, "column": at, "cards": cards}),
    ))
}

/// A repair belongs to the same group, before the original members, just
/// as it must be first in the next merge. No group exists in the coded flow.
fn join_fix_group(t: &mut Tx<'_>, ctx: &RunCtx, card: &str) -> ApiResult<()> {
    let Some(key) = &ctx.merge_group_key else {
        return Ok(());
    };
    let Some(g) = crate::store::groups::by_key(t, &ctx.project, key)? else {
        return Ok(());
    };
    if crate::store::groups::add(t, &g, card)? {
        t.execute(
            "UPDATE group_members SET position = (SELECT min(position) - 1 FROM group_members WHERE group_card = ?1) WHERE group_card = ?1 AND card = ?2",
            params![g.id, card],
        )?;
    }
    Ok(())
}

// ----------------------------------------------------------- agent verbs

/// `agent.park`, `agent.unpark`, `agent.end` on the open agent bound to
/// the run's card. No bound agent: a no-op.
fn agent_verb(
    t: &mut Tx<'_>,
    actor: &Actor,
    ctx: &RunCtx,
    a: &Args<'_>,
    effects: &mut Vec<Effect>,
) -> ApiResult<Done> {
    let verb = a.action.verb.as_str();
    let card = the_card(ctx, verb)?;
    policy::admin_project(actor, &card.project, "acting on the bound agent of a card")?;
    let bound = Bound::new(crate::bound::KIND_CARD, &card.id);
    let Some(sp) = spawns::open_bound(t, &bound)? else {
        return Ok(noop(
            json!({"card": card.id, "reason": format!("{} has no bound agent", card.id)}),
        ));
    };
    let detail = |extra: &str| json!({"card": card.id, "agent": sp.name, "result": extra});
    match verb {
        "agent.park" => {
            let reason = a
                .text("reason")
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| format!("rule {}", ctx.rule));
            Ok(match crate::bound::park_tx(t, &sp.id, &reason)? {
                Some(owner) => {
                    effects.push(Effect::RetryOwner(owner));
                    changed(detail("parked"))
                }
                None => noop(detail("parked already")),
            })
        }
        "agent.unpark" => {
            if sp.parked_at.is_none() {
                return Ok(noop(detail("not parked")));
            }
            let turn = unpark_turn(ctx, card, a);
            let (r, owner) = crate::bound::unpark_tx(t, &sp.id, Some(turn.as_str()), "rule")?;
            Ok(match r {
                crate::bound::Unparked::Woken => {
                    effects.push(Effect::Wake(sp.id.clone()));
                    changed(detail("woken"))
                }
                crate::bound::Unparked::Queued => {
                    effects.push(Effect::RetryOwner(owner));
                    changed(detail("queued"))
                }
                crate::bound::Unparked::Refused => changed(detail("refused")),
                crate::bound::Unparked::Nothing => noop(detail("nothing")),
            })
        }
        _ => {
            let outcome = match a.raw("outcome") {
                Some("cancelled") => spawns::CANCELLED,
                _ => spawns::DONE,
            };
            let result = a
                .text("result")
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| format!("the rule {} ended it ({outcome})", ctx.rule));
            effects.push(Effect::End {
                bound,
                outcome,
                result,
            });
            Ok(changed(detail(outcome)))
        }
    }
}

/// The turn of an unpark: the rule's `turn` (its values quoted), else the
/// return turn of the card move of the event, else a plain return turn.
fn unpark_turn(ctx: &RunCtx, card: &Card, a: &Args<'_>) -> String {
    if let Some(text) = a.text("turn").filter(|s| !s.trim().is_empty()) {
        return turn_tag::render(
            &TurnSource::System {
                kind: SystemKind::Card,
                detail: Some(card.id.clone()),
            },
            &text,
        );
    }
    match &ctx.event {
        Some(e) if e.kind == "card" && e.data["change"] == "moved" => {
            crate::bound::return_turn(card, e)
        }
        _ => {
            let working = ctx
                .columns
                .iter()
                .any(|c| c.name == card.column && c.role.as_deref() == Some(board::WORKING));
            crate::bound::return_text(
                card,
                &format!("the rule {} unparked it", ctx.rule),
                None,
                working,
            )
        }
    }
}
