//! The persistent store: one module per aggregate. Functions take a
//! `&Connection` to read or a `&mut Tx` to write; the API layer runs them
//! through [`crate::db::Db`].

pub mod agents;
pub mod artifacts;
pub mod audit;
pub mod board;
pub mod event_sources;
pub mod harness_updates;
pub mod hosts;
pub mod inbox;
pub mod leases;
pub mod mail;
pub mod memory;
pub mod monitors;
pub mod pointers;
pub mod projects;
pub mod questions;
pub mod records;
pub mod repos;
pub mod schedules;
pub mod session_refs;
pub mod spawns;
pub mod subscriptions;
pub mod sync;
pub mod targets;
pub mod templates;
pub mod test_runs;
pub mod transcripts;
pub mod usage;
pub mod versions;
pub mod waits;
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

pub fn check_name(kind: &str, name: &str) -> crate::error::ApiResult<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(crate::error::ApiError::invalid(format!(
            "bad {kind} name `{name}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or digit"
        )))
    }
}

/// Parse a JSON column, with a default for bad data.
pub fn json_col<T: serde::de::DeserializeOwned + Default>(text: &str) -> T {
    serde_json::from_str(text).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
