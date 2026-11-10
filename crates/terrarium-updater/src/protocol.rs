//! The wire format of the updater socket (docs/api.md §34.6).
//!
//! One connection carries one request and one answer, each one line of
//! JSON. The updater listens on `<dir>/updater.sock` (default
//! `/run/terrarium-updater/updater.sock`); the dir is `0700` and the socket
//! `0600`, and the updater refuses a peer with another uid.

use serde::{Deserialize, Serialize};

/// The default path of the updater socket.
pub const DEFAULT_SOCKET: &str = "/run/terrarium-updater/updater.sock";
/// The env var of the socket path (the daemon and the updater).
pub const ENV_SOCKET: &str = "TERRARIUM_UPDATER_SOCKET";
/// The longest request line that the updater reads.
pub const MAX_REQUEST: usize = 64 * 1024;

/// A request to the updater.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// The current image, the run in progress, and the last results.
    Status,
    /// Move the terrarium container to a release: a version (`0.3.0` or
    /// `v0.3.0`), `latest` (the newest stable release), or `pre` (the
    /// newest pre-release).
    Update {
        target: String,
        /// Who asked, for the record (`operator`, `agent:<ulid>`,
        /// `system:auto-update`).
        #[serde(default)]
        actor: String,
    },
    /// Go back to the image before the last switch, and restore the
    /// backup that the switch made.
    Rollback {
        #[serde(default)]
        actor: String,
    },
}

/// An answer of the updater.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Status(Box<Status>),
    /// The run started; `status` shows its steps.
    Accepted {
        run: String,
    },
    /// The updater refused the request. `busy`: a run is in progress.
    Error {
        message: String,
        #[serde(default)]
        busy: bool,
    },
}

/// What a run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Update,
    Rollback,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The run is in progress.
    Running,
    /// The container runs the new image and is healthy.
    Succeeded,
    /// The container ran that image already: nothing changed.
    Current,
    /// The run stopped before it changed the container (a bad tag, a
    /// failed verification, pull, drain, or backup). The old container
    /// runs on.
    Failed,
    /// The new image did not become healthy. The updater put the old
    /// image and the backup back, and the old container is healthy.
    RolledBack,
    /// The way back failed too. The operator must act.
    RollbackFailed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Current => "current",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
            Self::RollbackFailed => "rollback_failed",
        }
    }

    /// True for an outcome that changed nothing for the good.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failed | Self::RolledBack | Self::RollbackFailed)
    }
}

/// The state of one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Running,
    Ok,
    Failed,
    Skipped,
}

/// One step of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// `inspect`, `resolve`, `verify`, `pull`, `drain`, `backup`, `stop`,
    /// `restore`, `start`, `health`, `rollback`, ...
    pub name: String,
    pub state: StepState,
    /// One line about the step.
    pub detail: String,
    /// When the step started, RFC 3339 UTC.
    pub at: String,
}

/// An image as the updater knows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// The reference that made the container: `ghcr.io/...@sha256:...`,
    /// or a local tag (`terrarium:dev`, the image of a source deploy).
    pub reference: String,
    /// The image ID (`sha256:...`).
    pub id: String,
    /// The registry digest, when the image came from a registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// The `org.opencontainers.image.version` label, when the image has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One update or rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// A ULID.
    pub id: String,
    pub kind: RunKind,
    /// Who asked.
    pub actor: String,
    /// What the request named: a version, `latest`, `pre`, or `previous`.
    pub target: String,
    pub from: Option<Image>,
    pub to: Option<Image>,
    /// The backup that the run made before it stopped the container (a
    /// file name in the backup dir of the daemon).
    pub backup: Option<String>,
    pub outcome: Outcome,
    /// Why the run failed.
    pub error: Option<String>,
    pub steps: Vec<Step>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

/// The answer of [`Request::Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The build of the updater.
    pub updater_version: String,
    /// The image repository of the releases (`ghcr.io/wiltaylor/terrarium`).
    pub repository: String,
    /// The image check: `cosign`, `gh`, `cosign-keyless`, `command`, or `off`.
    pub verify: String,
    /// The image of the terrarium container now; `null` when the updater
    /// cannot find the container.
    pub current: Option<Image>,
    /// Why `current` is `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_error: Option<String>,
    /// The target of a rollback: the image before the last switch.
    pub previous: Option<Image>,
    /// The run in progress.
    pub running: Option<Run>,
    /// The last runs, newest first.
    pub history: Vec<Run>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged_json_lines() {
        let r = Request::Update {
            target: "latest".into(),
            actor: "operator".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"cmd":"update","target":"latest","actor":"operator"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"status"}"#).unwrap(),
            Request::Status
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"rollback"}"#).unwrap(),
            Request::Rollback { actor: "".into() }
        );
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"rm -rf"}"#).is_err());
    }

    #[test]
    fn answers_round_trip() {
        let a = Response::Error {
            message: "busy".into(),
            busy: true,
        };
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(s, r#"{"kind":"error","message":"busy","busy":true}"#);
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), a);
        assert_eq!(Outcome::RolledBack.as_str(), "rolled_back");
        assert!(Outcome::RollbackFailed.is_failure());
        assert!(!Outcome::Current.is_failure());
    }
}
