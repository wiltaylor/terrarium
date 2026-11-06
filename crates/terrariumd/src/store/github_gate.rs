//! The GitHub author gate (TER-85, docs/api.md §8.3, "GitHub author
//! gate").
//!
//! A project may list the GitHub logins whose issues count as work
//! (`github_authors` in `projects/<name>.toml`, or an operator `PATCH`).
//! An empty list: no gate. With a list, a GitHub card that an agent makes
//! (the label `github`, or a title that starts with `GitHub #<n>:`) needs
//! `github_author` from that list. Any other GitHub card is held: it goes
//! to the blocked column with the labels `github` and `hold`, whatever
//! column it asked for, and the daemon opens a question to the operator
//! for the creating agent, linked to the card.
//!
//! A held card leaves the blocked column, loses `hold`, or stops being a
//! GitHub card only by the operator, or when the operator answers its
//! question with [`RELEASE`]. An agent edit cannot turn a card into a
//! GitHub card, so GitHub work starts with `card new` and meets the gate.
//!
//! The gap: the author is what the creating agent says. The daemon does
//! not read the issue. A daemon-side issue sync that takes the author from
//! the signed webhook closes it (TER-124).

use rusqlite::{Connection, OptionalExtension};

use crate::error::{ApiError, ApiResult};

/// The label of a GitHub card.
pub const LABEL_GITHUB: &str = "github";
/// The label of a held card.
pub const LABEL_HOLD: &str = "hold";
/// The option of the hold question that releases the card to the backlog.
pub const RELEASE: &str = "Release to backlog";
/// The option of the hold question that keeps the card held.
pub const KEEP: &str = "Keep held";
/// The longest GitHub login.
const LOGIN_MAX: usize = 39;

fn has_label(labels: &[String], label: &str) -> bool {
    labels.iter().any(|l| l.trim().eq_ignore_ascii_case(label))
}

/// Does the title start with `GitHub #<n>:` (any case, spaces allowed
/// around `#<n>`)?
pub fn github_title(title: &str) -> bool {
    let t = title.trim_start();
    let Some(head) = t.get(..6) else {
        return false;
    };
    if !head.eq_ignore_ascii_case("github") {
        return false;
    }
    let Some(rest) = t[6..].trim_start().strip_prefix('#') else {
        return false;
    };
    let rest = rest.trim_start();
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && rest[digits..].trim_start().starts_with(':')
}

/// A GitHub card: the label `github`, or a `GitHub #<n>:` title.
pub fn is_github(title: &str, labels: &[String]) -> bool {
    has_label(labels, LABEL_GITHUB) || github_title(title)
}

/// A held card: a GitHub card with the label `hold`.
pub fn is_held(title: &str, labels: &[String]) -> bool {
    is_github(title, labels) && has_label(labels, LABEL_HOLD)
}

/// Is `author` on the allow list? GitHub logins have no case; a leading
/// `@` is no part of the login.
pub fn allowed(list: &[String], author: Option<&str>) -> bool {
    let Some(a) = author.map(|a| a.trim().trim_start_matches('@')) else {
        return false;
    };
    !a.is_empty() && list.iter().any(|l| l.eq_ignore_ascii_case(a))
}

/// Add `github` and `hold` to the labels of a held card.
pub fn hold_labels(labels: &mut Vec<String>) {
    for l in [LABEL_GITHUB, LABEL_HOLD] {
        if !has_label(labels, l) {
            labels.push(l.to_owned());
        }
    }
}

/// Take `hold` out of the labels.
pub fn without_hold(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .filter(|l| !l.trim().eq_ignore_ascii_case(LABEL_HOLD))
        .cloned()
        .collect()
}

/// Check and clean an allow list: GitHub logins (1 to 39 of `a-z`, `A-Z`,
/// `0-9`, `-`, not starting with `-`), a leading `@` dropped, no copies.
pub fn normalise_authors(list: &[String]) -> ApiResult<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for raw in list {
        let login = raw.trim().trim_start_matches('@');
        let ok = !login.is_empty()
            && login.len() <= LOGIN_MAX
            && !login.starts_with('-')
            && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !ok {
            return Err(ApiError::invalid(format!(
                "github_authors: `{raw}` is not a GitHub login (1-{LOGIN_MAX} of a-z, A-Z, \
                 0-9, -, not starting with -)"
            )));
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(login)) {
            out.push(login.to_owned());
        }
    }
    Ok(out)
}

/// The allow list of a project; empty: no gate.
pub fn authors(conn: &Connection, project: &str) -> ApiResult<Vec<String>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT github_authors FROM projects WHERE name = ?1",
            [project],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(raw
        .map(|r| crate::store::json_col::<Vec<String>>(&r))
        .unwrap_or_default())
}

/// The allow list as the `projects.github_authors` column: `NULL` for an
/// empty list.
pub fn authors_col(list: &[String]) -> Option<String> {
    (!list.is_empty()).then(|| serde_json::json!(list).to_string())
}

/// The text of the hold question.
pub fn question_text(card: &str, project: &str, author: Option<&str>) -> String {
    let who = match author.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("by GitHub user `{a}`, who is not on"),
        None => "with no GitHub author; the author must be on".to_owned(),
    };
    format!(
        "GitHub card {card} is held: it was made {who} the github_authors list of project \
         {project}. Release it to the backlog as work, or keep it held?"
    )
}

/// The comment of the move of a held card to the blocked column.
pub fn hold_comment(project: &str, author: Option<&str>) -> String {
    let who = match author.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("GitHub author `{a}` is not on"),
        None => "No GitHub author given; it must be on".to_owned(),
    };
    format!(
        "Held by the GitHub author gate: {who} the github_authors list of project {project}. \
         Only the operator releases this card."
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn v(x: &[&str]) -> Vec<String> {
        x.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_github_title_starts_with_the_issue_number() {
        assert!(github_title("GitHub #12: crash on start"));
        assert!(github_title("  github  # 7 : x"));
        assert!(github_title("GITHUB #1:"));
        assert!(!github_title("GitHub #: no number"));
        assert!(!github_title("GitHub 12: no hash"));
        assert!(!github_title("GitHub #12 no colon"));
        assert!(!github_title("Fix GitHub #12: not at the start"));
        assert!(!github_title("Git"));
        assert!(!github_title("Gïthub #1:"));
    }

    #[test]
    fn a_github_card_has_the_label_or_the_title() {
        assert!(is_github("x", &v(&["github"])));
        assert!(is_github("x", &v(&["GitHub"])));
        assert!(is_github("GitHub #3: y", &[]));
        assert!(!is_github("x", &v(&["gh", "bug"])));
        assert!(is_held("GitHub #3: y", &v(&["hold"])));
        assert!(!is_held("x", &v(&["hold"])));
    }

    #[test]
    fn the_author_must_be_on_the_list() {
        let list = v(&["wiltaylor", "wiltaylor-daf"]);
        assert!(allowed(&list, Some("wiltaylor")));
        assert!(allowed(&list, Some("@WilTaylor-DAF")));
        assert!(!allowed(&list, Some("mallory")));
        assert!(!allowed(&list, Some("")));
        assert!(!allowed(&list, None));
        assert!(!allowed(&[], Some("wiltaylor")));
    }

    #[test]
    fn hold_labels_are_added_once_and_removed() {
        let mut l = v(&["GitHub", "bug"]);
        hold_labels(&mut l);
        assert_eq!(l, v(&["GitHub", "bug", "hold"]));
        hold_labels(&mut l);
        assert_eq!(l.len(), 3);
        assert_eq!(without_hold(&l), v(&["GitHub", "bug"]));
    }

    #[test]
    fn the_allow_list_takes_logins_only() {
        assert_eq!(
            normalise_authors(&v(&["@wiltaylor", " wiltaylor-daf ", "WILTAYLOR"])).unwrap(),
            v(&["wiltaylor", "wiltaylor-daf"])
        );
        assert!(normalise_authors(&v(&["bad name"])).is_err());
        assert!(normalise_authors(&v(&["-x"])).is_err());
        assert!(normalise_authors(&v(&[""])).is_err());
        assert!(normalise_authors(&v(&[&"a".repeat(40)])).is_err());
        assert!(normalise_authors(&[]).unwrap().is_empty());
    }
}
