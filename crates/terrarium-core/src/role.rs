use serde::{Deserialize, Serialize};

/// A permission bundle on an agent. See PLAN §10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Every agent is a member of its project.
    #[default]
    Member,
    /// Administers one project.
    ProjectAdmin,
    /// Administers the whole system.
    Sysadmin,
}

/// A review decision on a version or a wiki page. See PLAN §15.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Reject,
    /// A comment that does not decide.
    Comment,
}
