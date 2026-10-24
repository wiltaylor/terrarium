//! Identifier newtypes. Terrarium IDs are ULIDs and serialise as strings.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

macro_rules! ulid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Ulid);

        impl $name {
            /// Make a new ID with the current time.
            pub fn new() -> Self {
                Self(Ulid::new())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ulid::from_string(s).map(Self)
            }
        }
    };
}

ulid_id!(
    /// A durable Terrarium agent identity.
    AgentId
);
ulid_id!(
    /// One harness conversation bound to an agent (Terrarium side).
    SessionId
);
ulid_id!(
    /// One mailbox item.
    MessageId
);
ulid_id!(
    /// A mail thread. Threads can span projects.
    ThreadId
);
ulid_id!(
    /// One normalised event.
    EventId
);
ulid_id!(
    /// One exec session in the executor protocol.
    ExecId
);

/// The session ID that the harness itself uses (often a UUID). It is opaque
/// to Terrarium.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HarnessSessionId(pub String);

impl fmt::Display for HarnessSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialise_as_strings() {
        let id = AgentId::new();
        let json = serde_json::to_string(&id).expect("serialise");
        assert_eq!(json, format!("\"{id}\""));
        let back: AgentId = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, id);
        assert_eq!(id.to_string().parse::<AgentId>().expect("parse"), id);
    }
}
