//! The persistent store: one module per aggregate. Functions take a
//! `&Connection` to read or a `&mut Tx` to write; the API layer runs them
//! through [`crate::db::Db`].

pub mod agents;
pub mod artifacts;
pub mod audit;
pub mod board;
pub mod calendar;
pub mod card_files;
pub mod card_history;
pub mod event_sources;
pub mod github_gate;
pub mod groups;
pub mod harness_updates;
pub mod hosts;
pub mod inbox;
pub mod jobs;
pub mod leases;
pub mod mail;
pub mod memory;
pub mod monitors;
pub mod pins;
pub mod pipelines;
pub mod pointers;
pub mod project_folders;
pub mod projects;
pub mod questions;
pub mod records;
pub mod repos;
pub mod reservations;
pub mod rules;
pub mod schedules;
pub mod session_refs;
pub mod spawns;
pub mod subscriptions;
pub mod sync;
pub mod target_grants;
pub mod targets;
pub mod templates;
pub mod test_runs;
pub mod threads;
pub mod tools;
pub mod transcripts;
pub mod usage;
pub mod version_groups;
pub mod waits;
pub mod webhooks;
pub mod wiki;

use serde::Deserialize;

/// `?limit=&cursor=` on list endpoints.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Page {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

impl Page {
    pub const DEFAULT: usize = 100;
    pub const MAX: usize = 1000;

    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(Self::DEFAULT).clamp(1, Self::MAX)
    }

    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref().filter(|c| !c.is_empty())
    }
}

/// One page of a list, and the cursor of the next page.
#[derive(Debug, Clone)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

impl<T> Paged<T> {
    /// `rows` holds up to `limit + 1` items. The extra one, when present,
    /// means there is a next page; the cursor is the key of the last item.
    pub fn from_rows(mut rows: Vec<T>, limit: usize, key: impl Fn(&T) -> String) -> Self {
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = if more { rows.last().map(key) } else { None };
        Self { items: rows, next }
    }
}

/// Names of projects and agents: lower-case letters, digits, `-` and `_`,
/// 1 to 63 characters, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The name of a new agent (create, spawn): [`check_name`], and not the
/// bare name of a project service. The services' holders are
/// `<project>/@<service>`, which no name can take (no `@`); the bare names
/// are reserved too, so no new agent can look like one. An agent made
/// before keeps its name (and only its own scope).
pub fn check_new_agent_name(name: &str) -> crate::error::ApiResult<()> {
    if terrarium_host::native::PROJECT_SERVICES.contains(&name) {
        return Err(crate::error::ApiError::invalid(format!(
            "agent name `{name}` is reserved for a project service"
        )));
    }
    check_name("agent", name)
}

pub fn check_name(kind: &str, name: &str) -> crate::error::ApiResult<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(crate::error::ApiError::invalid(format!(
            "bad {kind} name `{name}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or digit"
        )))
    }
}

/// A label that a test run or an artifact links to (TER-58), such as a
/// version label `version:vnext` or `version:1.4.0`. Free text: trimmed,
/// 1 to 200 characters, no control characters. The daemon does not check
/// that a card has it.
pub fn link_label(raw: &str) -> crate::error::ApiResult<String> {
    let label = raw.trim();
    if label.is_empty() {
        return Err(crate::error::ApiError::invalid("a label is empty"));
    }
    if label.chars().count() > 200 || label.chars().any(char::is_control) {
        return Err(crate::error::ApiError::invalid(format!(
            "bad label `{}`: use 1-200 characters, no control characters",
            label.escape_debug()
        )));
    }
    Ok(label.to_owned())
}

/// Parse a JSON column, with a default for bad data.
pub fn json_col<T: serde::de::DeserializeOwned + Default>(text: &str) -> T {
    serde_json::from_str(text).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// No agent can take a project service's holder (`<project>/@jobs`):
    /// `@` is never in a name, and the bare service names are reserved for
    /// new agents (create, spawn).
    #[test]
    fn agent_names_never_pass_for_a_project_service() {
        for service in terrarium_host::native::PROJECT_SERVICES {
            assert!(check_new_agent_name(service).is_err(), "{service}");
            assert!(check_new_agent_name(&format!("@{service}")).is_err());
            assert!(check_name("agent", &format!("@{service}")).is_err());
            // An agent made before the reservation keeps working (its
            // target root, its updates), and other kinds keep the name.
            check_name("agent", service).unwrap();
            check_name("project", service).unwrap();
        }
        check_new_agent_name("jobs-1").unwrap();
    }

    #[test]
    fn names() {
        assert!(valid_name("terrarium"));
        assert!(valid_name("a-b_1"));
        assert!(!valid_name("-a"));
        assert!(!valid_name("A"));
        assert!(!valid_name(""));
        assert!(!valid_name("a/b"));
    }

    #[test]
    fn paging() {
        let p = Paged::from_rows(vec![1, 2, 3], 2, |n| n.to_string());
        assert_eq!(p.items, [1, 2]);
        assert_eq!(p.next.as_deref(), Some("2"));
        let p = Paged::from_rows(vec![1, 2], 2, |n| n.to_string());
        assert_eq!(p.next, None);
    }
}
