//! Terrarium core: the domain model, events, the adapter trait, the executor
//! protocol, and the on-disk configuration definitions.
//!
//! This crate has no I/O apart from reading configuration files. The daemon,
//! the CLI, the host agent, and the adapters all depend on it.

pub mod adapter;
pub mod agent_template;
pub mod ci;
pub mod config;
pub mod display;
pub mod error;
pub mod event;
pub mod fleet;
pub mod harness;
pub mod harness_settings;
pub mod harnesses;
pub mod hook;
pub mod ids;
pub mod launch;
pub mod model;
pub mod moonshot;
pub mod paths;
pub mod protocol;
pub mod redact;
pub mod role;
pub mod schedule;
pub mod secret_scan;
pub mod skills;
pub mod status;
pub mod tail;
pub mod template;
pub mod transcript;
pub mod turn_tag;
pub mod zai;

pub use adapter::{Adapter, Answer, ScreenHint};
pub use error::{Error, Result};
pub use event::{Event, EventRecord};
pub use harness::HarnessId;
pub use hook::HookPayload;
pub use ids::{AgentId, EventId, HarnessSessionId, MessageId, SessionId, ThreadId};
pub use launch::{FileOverlay, LaunchSpec};
pub use model::Agent;
pub use role::{Decision, Role};
pub use status::AgentStatus;
pub use transcript::{Block, Transcript, TranscriptLog};

/// The version of the Terrarium crates, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
