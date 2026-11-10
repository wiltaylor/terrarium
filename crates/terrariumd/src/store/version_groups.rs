//! Accepted-card snapshots. A cut does no Git work. Only an explicit,
//! project-admin built acknowledgement completes a snapshot.
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::json;

use crate::actor::Actor;
use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::rules_verbs::BatchPick;
use crate::store::{board, groups, mail, threads};

pub const STEPS: &[&str] = &[
    "gathering",
    "cut",
    "version branch e2e",
    "pre-release",
    "done",
];

/// Ignore group cards, held cards, and cards still owned by a merge group.
/// This selection is shared by the timer and the transactional cut.
pub fn pick(conn: &Connection, project: &str) -> ApiResult<BatchPick> {
    let accepted = board::role_name(conn, project, "accepted", "accepted")?;
    let mut pick = BatchPick::default();
    for c in board::get_board(conn, project)?.cards {
        if c.column != accepted
            || c.kind == "group"
            || !c.labels.iter().any(|l| l == "version:vnext")
            || board::is_held(conn, &c.id)?
            || groups::for_card(conn, &c.id)?.is_some()
        {
            continue;
        }
        let since = board::last_moved_at(conn, &c.id)?.unwrap_or_else(|| c.created_at.clone());
        if let Ok(at) = DateTime::parse_from_rfc3339(&since) {
            let at = at.with_timezone(&Utc);
            pick.first_since = Some(pick.first_since.map_or(at, |old| old.min(at)));
        }
        pick.cards.push(c);
    }
    pick.waiting = pick.cards.len();
    Ok(pick)
}

pub fn cut(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    rule: &str,
    min_cards: i64,
    wait_minutes: i64,
) -> ApiResult<Option<String>> {
    policy::admin_project(actor, project, "cutting a version group")?;
    let pick = pick(t, project)?;
    let wait = chrono::Duration::try_minutes(wait_minutes)
        .filter(|_| wait_minutes > 0)
        .ok_or_else(|| ApiError::invalid("version wait is out of range"))?;
    if min_cards < 1 {
        return Err(ApiError::invalid("min_cards must be positive"));
    }
    let due = pick.first_since.and_then(|at| at.checked_add_signed(wait));
    if pick.cards.is_empty()
        || (pick.waiting < min_cards as usize && due.is_none_or(|at| at > Utc::now()))
    {
        return Ok(None);
    }
    // Eligibility is checked under the write lock. Once added, members
    // cannot be picked again. A new lifecycle gets its own immutable key.
    let key = format!("version:{}", ulid::Ulid::new());
    let accepted = board::role_name(t, project, "accepted", "accepted")?;
    let done = board::role_name(t, project, "done", "done")?;
    if !groups::open(
        t,
        groups::Open {
            project,
            rule,
            key: &key,
            title: &format!("Version cut: {} accepted cards", pick.waiting),
            column: &accepted,
            done_when: &done,
            eta: None,
            close_after: None,
        },
    )? {
        return Ok(None);
    }
    let g =
        groups::by_key(t, project, &key)?.ok_or_else(|| ApiError::internal("cut group missing"))?;
    for c in &pick.cards {
        groups::add(t, &g, &c.id)?;
    }
    for step in STEPS {
        groups::step(
            t,
            &g,
            step,
            Some(if *step == "gathering" {
                "done"
            } else if *step == "cut" {
                "running"
            } else {
                "pending"
            }),
            None,
            None,
        )?;
    }
    // Gathering happened before this snapshot was cut, not in zero seconds.
    if let Some(since) = pick.first_since {
        t.execute(
            "UPDATE group_steps SET started_at = ?2 WHERE group_card = ?1 AND name = 'gathering'",
            rusqlite::params![g.id, crate::time_param::stored(since)],
        )?;
    }
    groups::status(
        t,
        &g,
        Some("Membership frozen. Waiting for the lead to number and build the release."),
        None,
    )?;
    let to = threads::lead_of(t, project)?
        .map(|id| mail::resolve(t, &id, Some(project)))
        .transpose()?
        .unwrap_or(mail::Address::Operator);
    let members = groups::get(t, &g.id)?.members;
    let body = format!(
        "Version group {} (key {key}) freezes these accepted cards: {}. Pick the version number, relabel only these members, create the release branch, run e2e and publish the pre-release. Record steps with terrarium group status {key} --step 'version branch e2e' --state running. Remove excluded members with terrarium group remove {key} CARD. After release CI publishes, acknowledge with terrarium group built {key} vX.Y.Z. Use --project {project} if needed. No Git work has been performed by this rule.",
        g.id,
        members.join(", ")
    );
    crate::api::mail::send_checked(
        t,
        actor,
        &mail::Address::Project(project.into()),
        &to,
        &format!("{}: version cut ready", g.id),
        &body,
        None,
    )?;
    Ok(Some(key))
}

/// Add a late accepted card to an open version group (TER-608). The card
/// must be in the accepted column and in no other open group.
pub fn add(t: &mut Tx<'_>, actor: &Actor, project: &str, key: &str, card: &str) -> ApiResult<bool> {
    policy::admin_project(actor, project, "adding a version group member")?;
    let g = get(t, project, key)?;
    if g.closed_at.is_some() {
        return Err(ApiError::conflict("version group is closed"));
    }
    let accepted = board::role_name(t, project, "accepted", "accepted")?;
    let c = board::get_card(t, card)?;
    if c.project == project && !c.archived && c.column != accepted {
        return Err(ApiError::conflict(format!(
            "{card} must be in {accepted} to join a version group"
        )));
    }
    let changed = groups::add(t, &g, card)?;
    if changed {
        t.audit(
            "version.member_added",
            &g.id,
            Some(project),
            json!({"card":card}),
        )?;
    }
    Ok(changed)
}

/// `version` without a leading `v` before a digit: `v1.2.3` and `1.2.3`
/// are the same version.
pub fn bare(version: &str) -> &str {
    match version.strip_prefix('v') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => version,
    }
}

/// The `version:` labels that name `version`, in both forms
/// (`version:X.Y.Z` and `version:vX.Y.Z`) whichever form is given.
/// `version:vnext` never matches (it is the open version).
pub fn labels_for(version: &str) -> Vec<String> {
    let b = bare(version);
    let mut out = if b.starts_with(|c: char| c.is_ascii_digit()) {
        vec![format!("version:{b}"), format!("version:v{b}")]
    } else {
        vec![format!("version:{version}")]
    };
    out.retain(|l| l != "version:vnext");
    out
}

/// The versions of a card's `version:` labels, in the bare form, without
/// `vnext`.
pub fn shipped_label_versions(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .filter_map(|l| l.strip_prefix("version:"))
        .filter(|v| *v != "vnext" && !v.is_empty())
        .map(|v| bare(v).to_owned())
        .collect()
}

/// The versions (bare form) that `group built` acknowledged in `project`:
/// the closed version groups with a `Built <version>` status. The daemon
/// sees no Git tags, so this acknowledgement is its record that the
/// version tag is published. It reads `card_groups` by its
/// `(project, key)` index, not the event log.
pub fn built_versions(
    conn: &Connection,
    project: &str,
) -> ApiResult<std::collections::BTreeSet<String>> {
    let mut q = conn.prepare(
        "SELECT status FROM card_groups WHERE project = ?1 AND key LIKE 'version:%'
            AND closed_at IS NOT NULL AND status LIKE 'Built %'",
    )?;
    let rows = q
        .query_map([project], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .iter()
        .filter_map(|s| s.strip_prefix("Built "))
        .map(|v| bare(v).to_owned())
        .collect())
}

/// Acknowledge a built version. Moves the members, then every other
/// active, unheld accepted card labelled with that version and in no open
/// group, to done. Returns whether the group changed and the extra cards.
pub fn built(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    key: &str,
    version: &str,
) -> ApiResult<(bool, Vec<String>)> {
    policy::admin_project(actor, project, "acknowledging a built version")?;
    let g = get(t, project, key)?;
    if version.trim().is_empty() || version.len() > 128 || version.chars().any(char::is_control) {
        return Err(ApiError::invalid(
            "version must be nonempty, at most 128 bytes, with no control characters",
        ));
    }
    let status = format!("Built {version}");
    if g.closed_at.is_some() {
        return if g.status == status {
            Ok((false, Vec::new()))
        } else {
            Err(ApiError::conflict("group is already closed"))
        };
    }
    let accepted = board::role_name(t, project, "accepted", "accepted")?;
    let done = board::role_name(t, project, "done", "done")?;
    for id in &g.members {
        let c = board::get_card(t, id)?;
        if c.archived || board::is_held(t, id)? || (c.column != accepted && c.column != done) {
            return Err(ApiError::conflict(format!(
                "{id} must be active, unheld and accepted or done before acknowledging the build"
            )));
        }
    }
    for id in &g.members {
        if board::get_card(t, id)?.column == accepted {
            board::move_card(t, actor, id, &done, None, Some(&status))?;
        }
        board::assign_card(t, actor, id, None)?;
    }
    // Cards that joined the version after the cut, by label (TER-608).
    let names = labels_for(version);
    let mut extra = Vec::new();
    for c in board::get_board(t, project)?.cards {
        if c.column != accepted
            || c.kind != "card"
            || g.members.contains(&c.id)
            || !c.labels.iter().any(|l| names.contains(l))
            || board::is_held(t, &c.id)?
            || groups::for_card(t, &c.id)?.is_some()
        {
            continue;
        }
        board::move_card(t, actor, &c.id, &done, None, Some(&status))?;
        board::assign_card(t, actor, &c.id, None)?;
        extra.push(c.id);
    }
    for step in STEPS {
        groups::step(t, &g, step, Some("done"), None, None)?;
    }
    groups::status(t, &g, Some(&status), None)?;
    groups::close(t, &g, &status)?;
    t.audit(
        "version.built",
        &g.id,
        Some(project),
        json!({"key":key,"version":version,"also_done":extra}),
    )?;
    t.emit(
        crate::events::EventDraft::new(
            "version.built",
            &status,
            json!({"key":key,"version":version,"members":g.members,"also_done":extra}),
        )
        .project(project)
        .card(&g.id),
    )?;
    Ok((true, extra))
}

pub fn get(conn: &Connection, project: &str, key: &str) -> ApiResult<groups::Group> {
    let g = groups::by_key(conn, project, key)?
        .ok_or_else(|| ApiError::not_found("no such version group"))?;
    // Require the lifecycle marker as well as the namespace. An arbitrary
    // rule-created group must not become a release completion target.
    if !key.starts_with("version:") || !g.steps.iter().any(|s| s.name == "pre-release") {
        return Err(ApiError::invalid("not a version group"));
    }
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TER-608: either form of a version names both label forms.
    #[test]
    fn labels_for_matches_both_forms_from_either_form() {
        let both = vec!["version:1.2.3".to_owned(), "version:v1.2.3".to_owned()];
        assert_eq!(labels_for("v1.2.3"), both);
        assert_eq!(labels_for("1.2.3"), both);
        assert!(labels_for("vnext").is_empty());
        assert_eq!(labels_for("beta"), vec!["version:beta".to_owned()]);
        assert_eq!(
            shipped_label_versions(&["version:v0.36.0".into(), "version:vnext".into()]),
            vec!["0.36.0".to_owned()]
        );
    }
}
