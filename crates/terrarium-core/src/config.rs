//! On-disk TOML definitions: projects, agents, targets, and subscriptions.
//!
//! The daemon reads these from `$TERRARIUM_CONFIG_DIR` (default
//! `/etc/terrarium`), one file per object, in the `projects/`, `agents/`,
//! `targets/`, and `subscriptions/` directories. When a file has no `name`,
//! the file stem is the name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::harness::HarnessId;
use crate::role::Role;

/// What the agent may reach. See PLAN §9 and §12.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Project targets and global targets, by name.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Mail recipients. `"*"` means every agent in the same project.
    #[serde(default = "default_mail_to")]
    pub mail_to: Vec<String>,
    /// Explicit `project/agent` names outside the project.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cross_project: Vec<String>,
    /// The right to spawn ephemeral agents (PLAN §18, "Ephemeral
    /// agents"). Absent: the agent may not spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Boxed: most agents have none, and an actor carries the policy.
    pub spawn: Option<Box<SpawnPolicy>>,
    /// The right to add events to the project calendar, and to change and
    /// remove the events that the agent made (docs/api.md §33). A lead
    /// (`project_admin`) grants it; absent: the agent reads the calendar
    /// only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub calendar_write: bool,
    /// The right `thread:open` (docs/api.md §6.13): the agent opens Inbox
    /// threads (`terrarium thread new`). Granted like the spawn rights. The
    /// template of a thread that the agent opens may not exceed the
    /// agent's own rights. Absent: the agent opens no thread.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thread_open: bool,
}

/// The default of [`SpawnPolicy::max_concurrent`].
pub const SPAWN_DEFAULT_CONCURRENT: u32 = 2;
/// The default lifetime of an ephemeral agent.
pub const SPAWN_DEFAULT_LIFETIME: std::time::Duration = std::time::Duration::from_secs(2 * 3600);
/// The longest lifetime of an ephemeral agent.
pub const SPAWN_MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
/// The longest life of a bound ephemeral agent (TER-74), and the default
/// of [`SpawnPolicy::max_bound_lifetime`].
pub const SPAWN_MAX_BOUND_LIFETIME: std::time::Duration =
    std::time::Duration::from_secs(30 * 24 * 3600);
/// The highest `max_concurrent` and `max_per_day`.
pub const SPAWN_MAX_COUNT: u32 = 50;

/// What an agent may spawn (PLAN §18, "Ephemeral agents"): the policy
/// block `spawn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnPolicy {
    /// The templates that the agent may spawn: a template name (the
    /// project's template, else the global one), `global:<name>`,
    /// `project:*` (each template of the project), `global:*` (each
    /// global template), or `*` (both).
    #[serde(default)]
    pub templates: Vec<String>,
    /// The ephemeral agents of this owner that may run at one time.
    #[serde(default = "default_spawn_concurrent")]
    pub max_concurrent: u32,
    /// The spawns in the last 24 hours. Absent: no daily limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_day: Option<u32>,
    /// The lifetime of each ephemeral agent: a duration such as `30m` or
    /// `2h`. Absent: 2 h. At most 24 h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime: Option<String>,
    /// The ephemeral agents may spawn too, with a copy of this policy
    /// that has `may_spawn_children = false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub may_spawn_children: bool,
    /// The agent may bind an ephemeral agent to an object (TER-74, a
    /// card): the bound agent lives until the object is finished, at most
    /// [`Self::max_bound_lifetime`], in place of `max_lifetime`. Default
    /// off. A child never gets it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bound: bool,
    /// The cap of the life of a bound ephemeral agent: a duration such as
    /// `3d`. Absent: 30 days. At most 30 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bound_lifetime: Option<String>,
}

fn default_spawn_concurrent() -> u32 {
    SPAWN_DEFAULT_CONCURRENT
}

impl Default for SpawnPolicy {
    fn default() -> Self {
        Self {
            templates: Vec::new(),
            max_concurrent: SPAWN_DEFAULT_CONCURRENT,
            max_per_day: None,
            max_lifetime: None,
            may_spawn_children: false,
            bound: false,
            max_bound_lifetime: None,
        }
    }
}

/// The template that a spawn names, for [`SpawnPolicy::allows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnTemplate<'a> {
    pub name: &'a str,
    /// True for a global template, false for a template of the project.
    pub global: bool,
}

impl SpawnPolicy {
    /// The lifetime of each ephemeral agent.
    pub fn lifetime(&self) -> std::result::Result<std::time::Duration, String> {
        let Some(text) = self
            .max_lifetime
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        else {
            return Ok(SPAWN_DEFAULT_LIFETIME);
        };
        let d = crate::eta::parse_duration(text.trim())
            .ok()
            .and_then(|d| d.to_std().ok())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                format!("spawn.max_lifetime `{text}` is not a duration such as 30m or 2h")
            })?;
        if d > SPAWN_MAX_LIFETIME {
            return Err(format!(
                "spawn.max_lifetime `{text}` is longer than the maximum, 24h"
            ));
        }
        Ok(d)
    }

    /// The cap of the life of a bound ephemeral agent (TER-74).
    pub fn bound_lifetime(&self) -> std::result::Result<std::time::Duration, String> {
        let Some(text) = self
            .max_bound_lifetime
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        else {
            return Ok(SPAWN_MAX_BOUND_LIFETIME);
        };
        let d = crate::eta::parse_duration(text.trim())
            .ok()
            .and_then(|d| d.to_std().ok())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                format!("spawn.max_bound_lifetime `{text}` is not a duration such as 12h or 3d")
            })?;
        if d > SPAWN_MAX_BOUND_LIFETIME {
            return Err(format!(
                "spawn.max_bound_lifetime `{text}` is longer than the maximum, 30d"
            ));
        }
        Ok(d)
    }

    /// Check the block: each template entry has a valid form, the counts
    /// are in range, and the lifetimes parse.
    pub fn check(&self) -> std::result::Result<(), String> {
        for t in &self.templates {
            check_spawn_entry(t)?;
        }
        if self.max_concurrent == 0 || self.max_concurrent > SPAWN_MAX_COUNT {
            return Err(format!(
                "spawn.max_concurrent must be 1 to {SPAWN_MAX_COUNT}, not {}",
                self.max_concurrent
            ));
        }
        if let Some(n) = self.max_per_day
            && (n == 0 || n > SPAWN_MAX_COUNT * 20)
        {
            return Err(format!(
                "spawn.max_per_day must be 1 to {}, not {n}",
                SPAWN_MAX_COUNT * 20
            ));
        }
        self.lifetime()?;
        self.bound_lifetime().map(|_| ())
    }

    /// Does an entry of `templates` allow the template `t`?
    pub fn allows(&self, t: SpawnTemplate<'_>) -> bool {
        self.templates.iter().any(|e| {
            let e = e.trim();
            match e {
                "*" => true,
                "project:*" => !t.global,
                "global:*" => t.global,
                _ => match e.strip_prefix(crate::agent_template::GLOBAL_PREFIX) {
                    Some(n) => t.global && n == t.name,
                    None => e == t.name,
                },
            }
        })
    }

    /// The policy of a child: a copy with `may_spawn_children` and
    /// `bound` off.
    pub fn for_child(&self) -> Self {
        Self {
            may_spawn_children: false,
            bound: false,
            ..self.clone()
        }
    }
}

/// Check one entry of `spawn.templates`.
fn check_spawn_entry(e: &str) -> std::result::Result<(), String> {
    let e = e.trim();
    if matches!(e, "*" | "project:*" | "global:*") {
        return Ok(());
    }
    let name = e
        .strip_prefix(crate::agent_template::GLOBAL_PREFIX)
        .unwrap_or(e);
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "spawn.templates entry `{e}` must be a template name, `global:<name>`, \
             `project:*`, `global:*`, or `*`"
        ))
    }
}

fn default_mail_to() -> Vec<String> {
    vec!["*".to_owned()]
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            mail_to: default_mail_to(),
            cross_project: Vec::new(),
            spawn: None,
            calendar_write: false,
            thread_open: false,
        }
    }
}

/// What the supervisor does when the harness exits. See PLAN §18.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    #[default]
    OnFailure,
    Never,
}

/// How long an agent may rest (`idle` or `waiting`) before the daemon
/// stops it (PLAN §18, "Idle stop"). The text form is `off`, or a duration
/// of one or more `<number><unit>` parts with the units `d`, `h`, `m`, and
/// `s` (`30m`, `1h30m`, `90s`). `0` is `off`. A definition without the key
/// takes the global default (`TERRARIUM_IDLE_STOP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdleStop {
    /// The agent never stops for idleness.
    Off,
    /// The agent stops after this long at rest.
    After(std::time::Duration),
}

impl IdleStop {
    /// The time at rest before the stop; `None` for `off`.
    pub fn duration(self) -> Option<std::time::Duration> {
        match self {
            Self::Off => None,
            Self::After(d) => Some(d),
        }
    }

    /// The text form: `off`, or the duration in its largest units
    /// (`5400 s` is `1h30m`).
    pub fn as_text(self) -> String {
        let Self::After(d) = self else {
            return "off".into();
        };
        let mut secs = d.as_secs();
        let mut out = String::new();
        for (unit, size) in [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)] {
            if secs >= size {
                out.push_str(&format!("{}{unit}", secs / size));
                secs %= size;
            }
        }
        if out.is_empty() { "off".into() } else { out }
    }
}

impl std::str::FromStr for IdleStop {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let t = s.trim().to_ascii_lowercase();
        if t == "off" || t == "0" || t == "never" {
            return Ok(Self::Off);
        }
        crate::eta::parse_duration(&t)
            .map_err(|_| {
                format!("`{s}` is not an idle stop: use `off`, or a duration such as 30m or 1h30m")
            })
            .and_then(|d| {
                d.to_std()
                    .map(Self::After)
                    .map_err(|_| format!("`{s}` is not an idle stop"))
            })
    }
}

impl std::fmt::Display for IdleStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_text())
    }
}

impl Serialize for IdleStop {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_text())
    }
}

impl<'de> Deserialize<'de> for IdleStop {
    /// A text form, or a number of seconds (`0` is `off`).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Secs(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Secs(0) => Ok(Self::Off),
            Raw::Secs(n) => Ok(Self::After(std::time::Duration::from_secs(n))),
            Raw::Text(t) => t.parse().map_err(serde::de::Error::custom),
        }
    }
}

/// How old the context of a harness session may get before the daemon
/// compacts it at the next rest of the agent (PLAN §18, "Auto compact").
/// The age counts from the last compaction, else from the start of the
/// session. The text form is that of [`IdleStop`]: `off` (or `0`), or a
/// duration such as `24h`. A definition without the key takes the global
/// default (`TERRARIUM_COMPACT_MAX_AGE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompactMaxAge {
    /// No compaction for age.
    Off,
    /// Compact a session older than this at the next rest.
    After(std::time::Duration),
}

impl CompactMaxAge {
    /// The maximum age; `None` for `off`.
    pub fn duration(self) -> Option<std::time::Duration> {
        match self {
            Self::Off => None,
            Self::After(d) => Some(d),
        }
    }

    /// The text form: `off`, or the duration in its largest units.
    pub fn as_text(self) -> String {
        match self {
            Self::Off => "off".into(),
            Self::After(d) => IdleStop::After(d).as_text(),
        }
    }
}

impl std::str::FromStr for CompactMaxAge {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.parse::<IdleStop>() {
            Ok(IdleStop::Off) => Ok(Self::Off),
            Ok(IdleStop::After(d)) => Ok(Self::After(d)),
            Err(_) => Err(format!(
                "`{s}` is not a compact max age: use `off`, or a duration such as 24h or 12h30m"
            )),
        }
    }
}

impl std::fmt::Display for CompactMaxAge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_text())
    }
}

impl Serialize for CompactMaxAge {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_text())
    }
}

impl<'de> Deserialize<'de> for CompactMaxAge {
    /// A text form, or a number of seconds (`0` is `off`).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Secs(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Secs(0) => Ok(Self::Off),
            Raw::Secs(n) => Ok(Self::After(std::time::Duration::from_secs(n))),
            Raw::Text(t) => t.parse().map_err(serde::de::Error::custom),
        }
    }
}

/// An agent definition, `agents/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub project: String,
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    /// The plan accounts that the agent may move between (PLAN §16.3):
    /// subscription names of the same provider that the harness can spend.
    /// `subscription` is the account the agent starts on. A swap, by the
    /// operator or by the quota policy, picks another member. Empty: no
    /// swap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscription_pool: Vec<String>,
    /// pi and codex take a model flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The prompt template name under `prompts/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The base system prompt (PLAN §8), with the same `{{...}}`
    /// variables as a prompt template. Set: it replaces the harness's
    /// default system prompt, and the rendered `prompt` template comes
    /// after it. Unset or empty: the harness keeps its default prompt, and
    /// the rendered template is appended to it. At most
    /// [`SYSTEM_PROMPT_MAX`] bytes. A file can give `system_prompt_file`
    /// instead (see [`inline_system_prompt_file`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// The persona prompt (PLAN §8.1): `leader`, `worker`, `none`, or the
    /// name of a file `<prompts dir>/<name>.md`. Unset: by role, `leader`
    /// for an agent with `project_admin` or `sysadmin`, else `worker`.
    /// The persona replaces the harness's default system prompt, as a
    /// `system_prompt` does; `system_prompt` wins over it. `none` keeps the
    /// harness's default prompt. See [`check_persona`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// The idle stop of the agent ([`IdleStop`]). `None`: the global
    /// default (`TERRARIUM_IDLE_STOP`, 10 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stop: Option<IdleStop>,
    /// Compact the session when the agent reaches its idle stop (PLAN §18,
    /// "Auto compact"). `None`: the global default
    /// (`TERRARIUM_COMPACT_ON_IDLE`, off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_on_idle: Option<bool>,
    /// The maximum age of the session context ([`CompactMaxAge`]). `None`:
    /// the global default (`TERRARIUM_COMPACT_MAX_AGE`, off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_max_age: Option<CompactMaxAge>,
    /// Remind the agent of its cards in the working column when it goes
    /// idle after a turn (PLAN §18, "Card reminder"). `None`: the global
    /// default (`TERRARIUM_CARD_REMINDER`, on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_reminder: Option<bool>,
    /// The skills of the agent from the skill library (PLAN §8.6): skill
    /// names, or `*` for every skill of the library. Default none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// The skills that the agent does not get (PLAN §8.6): a default-on
    /// skill, or a skill of `skills`. Default none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills_off: Vec<String>,
    /// Give the agent the default-on skills of the library (PLAN §8.6).
    /// False: only the skills of `skills`. Default true.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub default_skills: bool,
    /// Start a fresh session, not a resume, when the rendered prompt
    /// changed and the harness keeps the prompt of a session
    /// (`Adapter::prompt_frozen_on_resume`, for example Codex). The old
    /// session stays in the history. Default true.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub new_session_on_prompt_change: bool,
}

impl AgentDef {
    /// The skill settings of the definition (PLAN §8.6).
    pub fn skill_selection(&self) -> crate::skills::Selection<'_> {
        crate::skills::Selection {
            skills: &self.skills,
            off: &self.skills_off,
            default_skills: self.default_skills,
        }
    }
}

fn yes() -> bool {
    true
}

/// The default of the most bytes in a system prompt: an agent's
/// `system_prompt`, or a persona file of the library. See
/// [`system_prompt_max`].
pub const SYSTEM_PROMPT_MAX: usize = 32 * 1024;

/// `TERRARIUM_SYSTEM_PROMPT_MAX`: the most bytes in a system prompt
/// (bytes, or a size with `K` or `M`, for example `64K`). Default
/// [`SYSTEM_PROMPT_MAX`].
pub const ENV_SYSTEM_PROMPT_MAX: &str = "TERRARIUM_SYSTEM_PROMPT_MAX";

/// The highest value of [`ENV_SYSTEM_PROMPT_MAX`]. Claude Code gets the
/// system prompt as one command line argument, and Linux refuses an
/// argument over 128 KiB (`MAX_ARG_STRLEN`). The protocol template and the
/// harness section come after the base prompt in that argument, so the
/// base prompt keeps 32 KiB free for them.
pub const SYSTEM_PROMPT_MAX_CEILING: usize = 96 * 1024;

/// The value of [`ENV_SYSTEM_PROMPT_MAX`]: `None` or an empty text gives
/// [`SYSTEM_PROMPT_MAX`]. The error is the reason that the value is not
/// valid.
pub fn parse_system_prompt_max(value: Option<&str>) -> std::result::Result<usize, String> {
    let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(SYSTEM_PROMPT_MAX);
    };
    let n = crate::limits::bytes(&crate::limits::LimitValue::Text(v.to_owned()))
        .map_err(|e| format!("bad value `{v}`: {e}"))?;
    let n = usize::try_from(n).map_err(|_| format!("bad value `{v}`: too large"))?;
    if n == 0 {
        return Err(format!("bad value `{v}`: the limit must be more than 0"));
    }
    if n > SYSTEM_PROMPT_MAX_CEILING {
        return Err(format!(
            "bad value `{v}`: the highest limit is {SYSTEM_PROMPT_MAX_CEILING} bytes (96K)"
        ));
    }
    Ok(n)
}

/// The most bytes in a system prompt, from [`ENV_SYSTEM_PROMPT_MAX`]. The
/// process reads the variable one time. A value that is not valid gives
/// [`SYSTEM_PROMPT_MAX`], with a warning.
pub fn system_prompt_max() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        let v = std::env::var(ENV_SYSTEM_PROMPT_MAX).ok();
        parse_system_prompt_max(v.as_deref()).unwrap_or_else(|e| {
            tracing::warn!("{ENV_SYSTEM_PROMPT_MAX}: {e}; the limit is {SYSTEM_PROMPT_MAX} bytes");
            SYSTEM_PROMPT_MAX
        })
    })
}

/// The refusal of a system prompt of `len` bytes, or `None` when `len` is
/// in the limit ([`system_prompt_max`]). `what` names the prompt, for
/// example "`system_prompt`" or "the persona `big`".
pub fn system_prompt_too_large(what: &str, len: usize) -> Option<String> {
    let max = system_prompt_max();
    (len > max)
        .then(|| format!("{what} has {len} bytes; the limit is {max} ({ENV_SYSTEM_PROMPT_MAX})"))
}

/// The persona that keeps the harness's default system prompt.
pub const PERSONA_NONE: &str = "none";
/// The persona of an agent with `project_admin` or `sysadmin`.
pub const PERSONA_LEADER: &str = "leader";
/// The persona of a member.
pub const PERSONA_WORKER: &str = "worker";

/// Is `name` a persona that an agent may have? A persona is a file name
/// without `.md`: letters, digits, `-`, and `_`, at most 64 bytes. An
/// empty name is valid: it means "by role".
pub fn check_persona(name: &str) -> std::result::Result<(), String> {
    let ok = name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "the persona `{name}` is not valid: use `leader`, `worker`, `none`, or a prompt file name \
             (letters, digits, `-`, `_`)"
        ))
    }
}

/// A persona as the store keeps it: an empty name is `None` (by role).
pub fn clean_persona(name: Option<String>) -> std::result::Result<Option<String>, String> {
    match name.map(|n| n.trim().to_owned()) {
        Some(n) if n.is_empty() => Ok(None),
        Some(n) => {
            check_persona(&n)?;
            Ok(Some(n))
        }
        None => Ok(None),
    }
}

/// Is `model` a model name that an agent may have? The harnesses take it
/// as the value of their `--model` flag (TER-1192, rev-1192 F4), so a
/// value that starts with `-` would read as another flag, and white space
/// is never part of a model ID. An empty name is valid: it means "the
/// harness default".
pub fn check_model(model: &str) -> std::result::Result<(), String> {
    if model.starts_with('-') {
        return Err(format!(
            "the model `{model}` is not valid: a model name cannot start with `-`"
        ));
    }
    if model.chars().any(char::is_whitespace) {
        return Err(format!(
            "the model `{}` is not valid: a model name has no white space",
            model.escape_debug()
        ));
    }
    Ok(())
}

/// The key in a definition file that names a file with the system prompt.
pub const SYSTEM_PROMPT_FILE_KEY: &str = "system_prompt_file";

/// Is `text` a system prompt that the agent may have? An empty text is
/// valid: it means "no system prompt".
pub fn check_system_prompt(text: &str) -> std::result::Result<(), String> {
    match system_prompt_too_large("`system_prompt`", text.len()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A system prompt as an agent keeps it: checked, and `None` for a text
/// that is empty or only white space.
pub fn clean_system_prompt(text: Option<String>) -> std::result::Result<Option<String>, String> {
    match text {
        Some(t) if !t.trim().is_empty() => {
            check_system_prompt(&t)?;
            Ok(Some(t))
        }
        _ => Ok(None),
    }
}

/// Replace `system_prompt_file = "<path>"` in a definition table with
/// `system_prompt = "<the file text>"`. A relative path is relative to
/// `base`, the directory of the definition file. A table with both keys
/// is an error.
pub fn inline_system_prompt_file(
    table: &mut toml::Table,
    base: &Path,
) -> std::result::Result<(), String> {
    let Some(v) = table.remove(SYSTEM_PROMPT_FILE_KEY) else {
        return Ok(());
    };
    let toml::Value::String(rel) = v else {
        return Err(format!("`{SYSTEM_PROMPT_FILE_KEY}` must be a string"));
    };
    if table.contains_key("system_prompt") {
        return Err(format!(
            "set `system_prompt` or `{SYSTEM_PROMPT_FILE_KEY}`, not both"
        ));
    }
    let path = base.join(&rel);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "cannot read `{SYSTEM_PROMPT_FILE_KEY}` {}: {e}",
            path.display()
        )
    })?;
    check_system_prompt(&text)?;
    table.insert("system_prompt".into(), toml::Value::String(text));
    Ok(())
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_yes(b: &bool) -> bool {
    *b
}

/// A git remote attached to a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDef {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// A key in `/state/credentials`. Never given to agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The checkout directory name. The default is the repo name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A tracked dir of the repo (for example `.githooks`). At each clone
    /// and update, the daemon copies its files from `HEAD` into
    /// `.git/hooks` of the agent's checkout. See [`check_hooks_dir`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_dir: Option<String>,
    /// Branch globs that a push from an agent checkout must not touch
    /// (for example `op/*`). The daemon writes a `pre-push` hook that
    /// refuses them. See [`check_push_deny`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_deny: Vec<String>,
    /// How an agent checkout reaches the remote: through the daemon's git
    /// proxy, or straight to the forge. Absent: the proxy for an `https`
    /// repo with a token and a `push_deny` list (when the proxy is on),
    /// else direct. See [`PushVia`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_via: Option<PushVia>,
}

/// The path of an agent checkout to the forge (issue #11, PLAN §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushVia {
    /// `origin` is the daemon's git proxy. The proxy adds the forge token
    /// and refuses each pushed ref that matches `push_deny`; the agent
    /// never gets the token.
    Proxy,
    /// `origin` is the forge. The credential helper gives the token to
    /// git, and `push_deny` is only a `pre-push` hook.
    Direct,
}

impl PushVia {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Direct => "direct",
        }
    }

    /// Parse `proxy` or `direct`.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "proxy" => Ok(Self::Proxy),
            "direct" => Ok(Self::Direct),
            other => Err(format!(
                "bad `push_via` `{other}`: give `proxy` or `direct`"
            )),
        }
    }
}

/// Does `branch` (a name without `refs/heads/`) match the `push_deny` glob
/// `glob`? The rules of a shell `case` pattern, as the `pre-push` hook
/// uses it: `*` is any text (`/` too), `?` is one character, and each
/// other character is itself ([`check_push_deny_glob`] allows no other
/// pattern character).
pub fn push_deny_matches(glob: &str, branch: &str) -> bool {
    let g: Vec<char> = glob.chars().collect();
    let b: Vec<char> = branch.chars().collect();
    // The classic two-pointer match with one backtrack point for `*`.
    let (mut gi, mut bi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while bi < b.len() {
        match g.get(gi) {
            Some('*') => {
                star = Some((gi, bi));
                gi += 1;
            }
            Some('?') => {
                gi += 1;
                bi += 1;
            }
            Some(c) if *c == b[bi] => {
                gi += 1;
                bi += 1;
            }
            _ => match star {
                Some((sg, sb)) => {
                    gi = sg + 1;
                    bi = sb + 1;
                    star = Some((sg, sb + 1));
                }
                None => return false,
            },
        }
    }
    g[gi..].iter().all(|c| *c == '*')
}

/// The first glob of `list` that `branch` matches.
pub fn push_deny_match<'a>(list: &'a [String], branch: &str) -> Option<&'a str> {
    list.iter()
        .map(String::as_str)
        .find(|g| push_deny_matches(g, branch))
}

/// The most entries in a repo's `push_deny`.
pub const PUSH_DENY_MAX: usize = 64;

/// Is `dir` a `hooks_dir` that a repo may have? A path relative to the
/// root of the repo, with `/` between the parts, and no `.`, `..`, or
/// `.git` part. At most 200 bytes.
pub fn check_hooks_dir(dir: &str) -> std::result::Result<(), String> {
    let bad = |why: &str| Err(format!("bad `hooks_dir` `{dir}`: {why}"));
    if dir.is_empty() || dir.len() > 200 {
        return bad("give a dir of the repo, at most 200 bytes, for example `.githooks`");
    }
    if dir.starts_with('/') || dir.ends_with('/') {
        return bad("give a path relative to the root of the repo, with no `/` at the ends");
    }
    for part in dir.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return bad("a part is empty, `.`, or `..`");
        }
        if part.eq_ignore_ascii_case(".git") {
            return bad("a part is `.git`");
        }
        if part.chars().any(|c| c.is_control() || c == '\\') {
            return bad("a part has a control character or `\\`");
        }
    }
    Ok(())
}

/// Is `glob` an entry of `push_deny` that a repo may have? A branch name
/// glob without `refs/heads/`: letters, digits, and `.`, `_`, `-`, `/`,
/// `+`, `@`, with `*` (any text, `/` too) and `?` (one character). The
/// pre-push hook uses it as a shell `case` pattern, so no other character
/// is allowed. At most 200 bytes.
pub fn check_push_deny_glob(glob: &str) -> std::result::Result<(), String> {
    let ok = !glob.is_empty()
        && glob.len() <= 200
        && !glob.starts_with("refs/")
        && glob.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+' | '@' | '*' | '?')
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "bad `push_deny` entry `{glob}`: give a branch glob without `refs/heads/`, such as \
             `op/*`, with letters, digits, `.`, `_`, `-`, `/`, `+`, `@`, `*`, and `?`"
        ))
    }
}

/// [`check_push_deny_glob`] for each entry, and at most
/// [`PUSH_DENY_MAX`] entries.
pub fn check_push_deny(list: &[String]) -> std::result::Result<(), String> {
    if list.len() > PUSH_DENY_MAX {
        return Err(format!(
            "`push_deny` has {} entries; the limit is {PUSH_DENY_MAX}",
            list.len()
        ));
    }
    list.iter().try_for_each(|g| check_push_deny_glob(g))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckoutDef {
    /// Repos cloned into `$HOME/work/<repo>` when an agent is made.
    #[serde(default)]
    pub on_agent_create: Vec<String>,
}

/// The job queue config of a project (TER-71), stored per project in the
/// daemon (`PUT /api/projects/{name}/delivery`). With no `target` or no
/// `recipe`, the project has no queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DeliveryDef {
    /// Retained job log lifetime in seconds. Default 7 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_retention_secs: Option<u64>,
    /// Total completed log bytes retained per project. Default 1 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_retention_bytes: Option<u64>,
    /// Public SSH endpoints and pinned VM host key for daemon-owned Docker access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_ci: Option<DockerCiDef>,
    /// Daemon-consumed Docker SSH key references under DOCKER_CI_KEY
    /// and DOCKER_CI_RESET_KEY (both required when configured).
    /// Values must be @secret:<name>[.<field>], authorized by delivery
    /// run grants. The recipe receives only a Docker endpoint, never a key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub merge_secrets: BTreeMap<String, String>,
    /// Literal failure markers to extract from job logs. None uses Rust's
    /// FAILED, panicked, test result: FAILED, and failures: markers.
    /// An empty list disables extraction. Matching lines include three
    /// following lines so a panic's message is kept too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_patterns: Option<Vec<String>>,
    /// The target that runs the jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The validation command, run with `sh -c` in the root of the
    /// checkout, for example `just ci::validate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// The most jobs that run at the same time. Default 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<u32>,
    /// The repos (by name) that have a `next` branch for merge jobs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_repos: Vec<String>,
    /// The dir on the target under which each job gets a fresh clone.
    /// Default `/tmp/terrarium-jobs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<String>,
    /// The most time a job may run, in seconds. Default 3600.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// The release client target of release jobs (TER-623): the only
    /// target where `release_secrets` resolve. It runs release jobs only:
    /// it must not be the `target` of a job queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_target: Option<String>,
    /// The release command, run with `sh -c` in the root of a clean
    /// checkout of the release commit on `main`. Default
    /// [`DeliveryDef::DEFAULT_RELEASE_RECIPE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_recipe: Option<String>,
    /// The secrets of the release recipe's env (TER-623), by env name:
    /// `@secret:<name>[.<field>]` references, resolved only for a release
    /// job on `release_target`, through a `delivery:<project>` run grant
    /// that names that target. Never for a merge or validate job.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub release_secrets: BTreeMap<String, String>,
    /// The Docker endpoint of release jobs (TER-623): the release VM,
    /// reached through the daemon's relay. The recipe gets only
    /// `TERRARIUM_DOCKER_HOST`; the key stays in the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_release: Option<DockerReleaseDef>,
}

/// The release VM of release jobs (TER-623). No reset endpoint: the VM
/// keeps its build cache between releases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerReleaseDef {
    /// SSH destination, for example root@host. No SSH options are accepted.
    pub destination: String,
    pub port: u16,
    /// OpenSSH known_hosts line for destination and port.
    pub known_hosts: String,
    /// The SSH key of the VM: an `@secret:<name>[.<field>]` reference
    /// that the daemon resolves and uses itself, never recipe env.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerCiDef {
    /// SSH destination, for example root@host. No SSH options are accepted.
    pub destination: String,
    pub port: u16,
    /// OpenSSH known_hosts line for destination and port.
    pub known_hosts: String,
    /// Forced-command reset destination, pinned by reset_known_hosts.
    pub reset_destination: String,
    pub reset_port: u16,
    /// Administrator-verified OpenSSH pin for the forced reset endpoint.
    pub reset_known_hosts: String,
}

impl DeliveryDef {
    /// The default number of slots.
    pub const DEFAULT_SLOTS: u32 = 2;
    /// The default work dir on the target.
    pub const DEFAULT_WORK_DIR: &'static str = "/tmp/terrarium-jobs";
    /// The default job timeout.
    pub const DEFAULT_TIMEOUT_SECS: u64 = 3600;

    /// True when the project has a queue.
    pub fn enabled(&self) -> bool {
        self.target.as_deref().is_some_and(|t| !t.is_empty())
            && self.recipe.as_deref().is_some_and(|r| !r.trim().is_empty())
    }

    pub fn slots(&self) -> usize {
        self.slots.unwrap_or(Self::DEFAULT_SLOTS).max(1) as usize
    }

    pub fn work_dir(&self) -> &str {
        self.work_dir.as_deref().unwrap_or(Self::DEFAULT_WORK_DIR)
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(Self::DEFAULT_TIMEOUT_SECS)
            .max(1)
    }

    /// The default release command (docs/release.md).
    pub const DEFAULT_RELEASE_RECIPE: &'static str = "just release::publish";

    /// The release target, when one is set.
    pub fn release_target(&self) -> Option<&str> {
        self.release_target.as_deref().filter(|t| !t.is_empty())
    }

    /// The config that a release job runs with (TER-623): the release
    /// target and recipe in place of `target` and `recipe`, one slot, and
    /// no merge secrets, Docker CI endpoint, or `next` repos. `None`
    /// without a release target.
    pub fn release_view(&self) -> Option<DeliveryDef> {
        let target = self.release_target()?.to_owned();
        let recipe = self
            .release_recipe
            .clone()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| Self::DEFAULT_RELEASE_RECIPE.to_owned());
        Some(DeliveryDef {
            log_retention_secs: self.log_retention_secs,
            log_retention_bytes: self.log_retention_bytes,
            docker_ci: None,
            merge_secrets: BTreeMap::new(),
            failure_patterns: self.failure_patterns.clone(),
            target: Some(target),
            recipe: Some(recipe),
            slots: Some(1),
            next_repos: Vec::new(),
            work_dir: self.work_dir.clone(),
            timeout_secs: self.timeout_secs,
            release_target: self.release_target.clone(),
            release_recipe: self.release_recipe.clone(),
            release_secrets: self.release_secrets.clone(),
            docker_release: self.docker_release.clone(),
        })
    }
}

/// A project definition, `projects/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDef {
    /// Opt-in ready-card dispatcher owner (a project admin). `off` disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_dispatcher: Option<String>,
    /// Opt-in admin owner for review dispatch, or `off` to disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_dispatcher: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The card ID prefix, for example `TER` for `TER-42`. The default comes
    /// from the name. See PLAN §13.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoDef>,
    #[serde(default)]
    pub checkout: CheckoutDef,
    /// The project's schedules (docs/api.md §19): `[[schedules]]` in a
    /// project file, `[[projects.schedules]]` in a fleet file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<crate::schedule::ScheduleDef>,
    /// The project's agent templates (PLAN §5b): `[[templates]]` in a
    /// project file, `[[projects.templates]]` in a fleet file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<crate::agent_template::TemplateDef>,
    /// The GitHub author gate (TER-85): the GitHub logins whose issues
    /// count as work. A GitHub card from another author is held for the
    /// operator. Empty: no gate. Only a project file sets it (or the
    /// operator through the API). A fleet file that gives the key is an
    /// error (`fleet::project`), so an agent cannot change it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub github_authors: Vec<String>,
    /// Who runs the card flow of the job queue and of the bound agents
    /// (TER-170): the daemon's code (`code`, the default) or the board
    /// rules with the built-in pack (`rules`). Absent: unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_flow: Option<BoardFlow>,
}

/// Who runs the card flow of a project (TER-170, docs/api.md §19.11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoardFlow {
    /// The daemon's code moves, labels, and comments the cards of the job
    /// queue, and parks and ends the bound agents.
    #[default]
    Code,
    /// The board rules do it: the built-in pack and the project's
    /// `rules.wcl`. The code only emits the events.
    Rules,
    /// The rules of the board's columns do it (TER-1113, kanban rework C2):
    /// no pack, no `rules.wcl`. The code only emits the events. Only the
    /// upgrade that converts the boards (C5) sets it, so a project file
    /// cannot (`skip_deserializing`), and a sync does not change it back.
    #[serde(skip_deserializing)]
    Columns,
}

impl BoardFlow {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Rules => "rules",
            Self::Columns => "columns",
        }
    }

    /// Does the daemon's code leave the card flow to a rule engine (`rules`
    /// or `columns`)? Then the code only emits the events.
    pub fn engine_runs(self) -> bool {
        self != Self::Code
    }

    /// The flow of a stored word; anything else is `code`.
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("rules") => Self::Rules,
            Some("columns") => Self::Columns,
            _ => Self::Code,
        }
    }
}

/// The kind of a target. See PLAN §19.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Host,
    Ssh,
    Docker,
    Vmlab,
    /// A VNC server that the host agent connects to: display only
    /// (PLAN §19b).
    Vnc,
    /// An RDP server that the host agent connects to: display only.
    Rdp,
    /// The desktop of the host agent's own machine: display only, and only
    /// when the host agent's config opts in (`[display] enabled = true`).
    Desktop,
    /// An exe.dev VM that the daemon makes on `up` and removes on `down`
    /// (TER-294). Commands go over ssh; the lifecycle goes through the
    /// exe.dev HTTPS API. Always ephemeral. See [`ExedevSettings`].
    Exedev,
}

impl TargetKind {
    /// The kinds that have only a display: no exec, no copy, no shell.
    pub fn display_only(self) -> bool {
        matches!(self, Self::Vnc | Self::Rdp | Self::Desktop)
    }
}

/// A target definition, `targets/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetDef {
    /// Windows host account and the daemon-side login secret used at exec start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_run_as: Option<WindowsRunAs>,
    /// Card-agent build roots live under `<work_root>/<project>/<agent>`.
    /// Shared project caches must live outside these roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_root: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub kind: TargetKind,
    /// The owning project. A target without one is global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The host agent name, for `host`, `docker`, `vmlab`, `desktop`, and
    /// the host agent that connects to a `vnc` or `rdp` server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `ssh`: the address, `user@host` or `host`. `vnc` and `rdp`:
    /// `host:port` as the host agent reaches it (the port defaults to 5900
    /// and 3389).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// `ssh`: the key name in `/state/credentials`. `vnc` and `rdp`: the
    /// name of a `password` credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// `vnc` and `rdp`: the user name to log in with. Default: the
    /// `username` of the credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// `ssh`: mirror the caller's workspace before and after operations.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sync_workspace: bool,
    /// `rdp`: the Windows domain of the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// `docker` and `desktop`: the display server (`x11`, `wayland`, or
    /// `auto`). A docker target with it has a display (PLAN §19b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<crate::display::DisplayProtocol>,
    /// `docker` and `desktop`: the X display (default `:0`) or the Wayland
    /// socket (default `wayland-0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// `docker`: the image to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `docker`: an existing container to exec into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// `vmlab`: the lab file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab: Option<PathBuf>,
    /// `vmlab`: the machine in the lab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// `vmlab`: the name of a repo of the target's project that holds the
    /// lab definition. On `up` the host agent makes a git worktree of it at
    /// `lab_ref`, checks the lab against its allowlist, and runs it
    /// (docs/api.md §17.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_repo: Option<String>,
    /// `vmlab` with `lab_repo`: a branch, a tag, or a commit. Default: the
    /// repo's default branch, else `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_ref: Option<String>,
    /// `vmlab` with `lab_repo` or `lab_from_workspace`: the directory in the
    /// repo (or the workspace) that holds `vmlab.wcl`. Default: the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_path: Option<String>,
    /// `vmlab`: the lab definition is in the workspace of each holder
    /// (`<work>/<project>/<agent>/<lab_path>`). Each holder gets its own
    /// lab instance.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lab_from_workspace: bool,
    /// Created on agent start and destroyed on agent stop.
    #[serde(default)]
    pub ephemeral: bool,
    /// Keep an ephemeral target after the agent stops.
    #[serde(default)]
    pub keep: bool,
    #[serde(default)]
    pub snapshot_before_task: bool,
    /// `host` and `docker`: resource limits (docs/api.md §17.2, "Resource
    /// limits"). On a `host` target each holder gets its own budget; on a
    /// `docker` target the container has one budget for all holders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::limits::TargetLimits>,
    /// `exedev`: the VM settings (`[exedev]`). Default: [`ExedevSettings`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exedev: Option<ExedevSettings>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsRunAs {
    pub account: String,
    pub password_secret: String,
}

/// The `[exedev]` table of an `exedev` target (TER-294). Each `up` makes a
/// new exe.dev VM with these settings; `down` removes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExedevSettings {
    /// The name of the exe.dev API token secret (`POST /exec`): it needs
    /// `new`, `ls` and `rm`.
    #[serde(default = "ExedevSettings::default_api_token")]
    pub api_token: String,
    /// vCPUs of the VM (`new --cpu`).
    #[serde(default = "ExedevSettings::default_cpu")]
    pub cpu: u32,
    /// Memory of the VM in GiB (`new --memory=<n>GB`). At most 2 per vCPU.
    #[serde(default = "ExedevSettings::default_memory_gb")]
    pub memory_gb: u32,
    /// The tag of each VM. The ssh key of the target must be scoped to it.
    #[serde(default = "ExedevSettings::default_tag")]
    pub tag: String,
    /// The ssh user on the VM.
    #[serde(default = "ExedevSettings::default_user")]
    pub user: String,
    /// The life of a VM in seconds. The sweep removes a VM past it. At most
    /// 8 hours.
    #[serde(default = "ExedevSettings::default_max_life_secs")]
    pub max_life_secs: u64,
}

impl ExedevSettings {
    /// The longest life of a VM: 8 hours (operator, E4).
    pub const MAX_LIFE_SECS: u64 = 8 * 3600;

    fn default_api_token() -> String {
        "exedev-api-token".into()
    }
    fn default_cpu() -> u32 {
        2
    }
    fn default_memory_gb() -> u32 {
        4
    }
    fn default_tag() -> String {
        "terrarium-dev-ssh".into()
    }
    fn default_user() -> String {
        "exedev".into()
    }
    fn default_max_life_secs() -> u64 {
        Self::MAX_LIFE_SECS
    }
}

impl Default for ExedevSettings {
    fn default() -> Self {
        Self {
            api_token: Self::default_api_token(),
            cpu: Self::default_cpu(),
            memory_gb: Self::default_memory_gb(),
            tag: Self::default_tag(),
            user: Self::default_user(),
            max_life_secs: Self::default_max_life_secs(),
        }
    }
}

/// A subscription definition, `subscriptions/<name>.toml`. See PLAN §16.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The harnesses that can spend this subscription.
    #[serde(default)]
    pub harnesses: Vec<HarnessId>,
    /// How to read the remaining quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    /// Alert when any window passes this percentage.
    #[serde(default = "default_alert_at")]
    pub alert_at: u8,
    /// The server of a `selfhosted` subscription (`[selfhosted]`,
    /// [`crate::selfhosted`]). `None` for every other provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selfhosted: Option<crate::selfhosted::SelfHosted>,
    /// The settings of a cloud subscription (`[cloud]`, providers
    /// `bedrock`, `gcp`, and `foundry`; [`crate::cloud`]). `None` for
    /// every other provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<crate::cloud::CloudSettings>,
}

fn default_alert_at() -> u8 {
    80
}

/// A definition that has a name, which can come from the file stem.
pub trait Named {
    fn name_mut(&mut self) -> &mut String;

    /// Change the file's table before it is read: for example, read the
    /// files that it names. `base` is the directory of the file.
    fn prepare(_table: &mut toml::Table, _base: &Path) -> std::result::Result<(), String> {
        Ok(())
    }
}

macro_rules! named {
    ($($t:ty),*) => {$(
        impl Named for $t {
            fn name_mut(&mut self) -> &mut String {
                &mut self.name
            }
        }
    )*};
}
named!(TargetDef, SubscriptionDef, crate::tool::ToolDef);

impl Named for AgentDef {
    fn name_mut(&mut self) -> &mut String {
        &mut self.name
    }

    fn prepare(table: &mut toml::Table, base: &Path) -> std::result::Result<(), String> {
        inline_system_prompt_file(table, base)
    }
}

impl Named for ProjectDef {
    fn name_mut(&mut self) -> &mut String {
        &mut self.name
    }

    /// Each `[[templates]]` may name a `system_prompt_file`.
    fn prepare(table: &mut toml::Table, base: &Path) -> std::result::Result<(), String> {
        if let Some(toml::Value::Array(list)) = table.get_mut("templates") {
            for t in list.iter_mut().filter_map(toml::Value::as_table_mut) {
                inline_system_prompt_file(t, base)?;
            }
        }
        Ok(())
    }
}

/// Parse one definition file. The file stem fills in a missing name.
pub fn load_file<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Named,
{
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let toml_error = |source| Error::Toml {
        path: path.to_owned(),
        source: Box::new(source),
    };
    let mut table: toml::Table = toml::from_str(&text).map_err(toml_error)?;
    let base = path.parent().unwrap_or(Path::new("."));
    T::prepare(&mut table, base).map_err(|e| Error::Other(format!("{}: {e}", path.display())))?;
    let mut def: T = table.try_into().map_err(toml_error)?;
    let name = def.name_mut();
    if name.is_empty()
        && let Some(stem) = path.file_stem()
    {
        *name = stem.to_string_lossy().into_owned();
    }
    Ok(def)
}

/// Every `*.toml` in `dir`, sorted. A missing directory has no files.
fn toml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: dir.to_owned(),
                source,
            });
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml") && p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

fn load_kind<T>(dir: &Path, errors: &mut Vec<Error>) -> Vec<T>
where
    T: for<'de> Deserialize<'de> + Named,
{
    let files = match toml_files(dir) {
        Ok(files) => files,
        Err(e) => {
            errors.push(e);
            return Vec::new();
        }
    };
    files
        .iter()
        .filter_map(|path| match load_file(path) {
            Ok(def) => Some(def),
            Err(e) => {
                errors.push(e);
                None
            }
        })
        .collect()
}

/// All definitions found in a config directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Definitions {
    pub projects: Vec<ProjectDef>,
    pub agents: Vec<AgentDef>,
    pub targets: Vec<TargetDef>,
    pub subscriptions: Vec<SubscriptionDef>,
    /// `tools/<name>.toml`: the tool proxies (`crate::tool`).
    pub tools: Vec<crate::tool::ToolDef>,
    /// `projects/<project>.rules.wcl`: the board rules files (TER-166).
    /// The daemon checks them (`crate::rules_file`) when it syncs them.
    pub rules: Vec<RulesFile>,
}

/// A project's board rules file in the config dir, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesFile {
    pub project: String,
    pub text: String,
}

/// The extension of a rules file in `projects/`.
pub const RULES_SUFFIX: &str = ".rules.wcl";

/// The rules files in `dir` (`<project>.rules.wcl`), sorted by project.
fn load_rules(dir: &Path, errors: &mut Vec<Error>) -> Vec<RulesFile> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(source) => {
            errors.push(Error::Io {
                path: dir.to_owned(),
                source,
            });
            return Vec::new();
        }
    };
    let mut out: Vec<RulesFile> = Vec::new();
    for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
        let Some(project) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(RULES_SUFFIX))
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => out.push(RulesFile {
                project: project.to_owned(),
                text,
            }),
            Err(source) => errors.push(Error::Io { path, source }),
        }
    }
    out.sort_by(|a, b| a.project.cmp(&b.project));
    out
}

impl Definitions {
    /// Read every definition under `dir`. A file that does not parse goes
    /// into the error list and does not stop the load.
    pub fn load(dir: &Path) -> (Self, Vec<Error>) {
        let mut errors = Vec::new();
        let defs = Self {
            projects: load_kind(&dir.join("projects"), &mut errors),
            agents: load_kind(&dir.join("agents"), &mut errors),
            targets: load_kind(&dir.join("targets"), &mut errors),
            subscriptions: load_kind(&dir.join("subscriptions"), &mut errors),
            tools: load_kind(&dir.join("tools"), &mut errors),
            rules: load_rules(&dir.join("projects"), &mut errors),
        };
        (defs, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TER-1192 (rev-1192 F4): a model that would read as a flag, or that
    /// has white space, is refused.
    #[test]
    fn model_names_cannot_be_flags_or_have_white_space() {
        for ok in ["", "grok-build", "claude-haiku-4.5", "openai/gpt-6-luna", "m:1"] {
            assert!(check_model(ok).is_ok(), "{ok}");
        }
        for bad in ["-x", "--always-approve", "a b", "a\tb", "m\n--trust", " m"] {
            assert!(check_model(bad).is_err(), "{bad:?}");
        }
    }

    fn round_trip<T>(text: &str) -> T
    where
        T: for<'de> Deserialize<'de> + Serialize + PartialEq + std::fmt::Debug,
    {
        let parsed: T = toml::from_str(text).expect("parse example");
        let written = toml::to_string(&parsed).expect("serialise");
        let reparsed: T = toml::from_str(&written).expect("parse serialised");
        assert_eq!(parsed, reparsed);
        parsed
    }

    // PLAN §9, agent policy.
    const AGENT_POLICY: &str = r#"
project = "terrarium"
harness = "pi"
subscription = "anthropic-max"        # or "openai-codex", "copilot-pro", ...
model = "anthropic/claude-opus-5-5"   # optional; pi and codex take a model flag

[policy]
targets = ["buildbox", "lab-win11"]   # project targets and global targets by name
mail_to = ["*"]                       # "*" means every agent in the same project
# cross_project = ["ops/triage"]      # explicit project/agent names outside the project
"#;

    // PLAN §10, a leader agent.
    const AGENT_LEAD: &str = r#"
# agents/terrarium-lead.toml
name = "lead"
project = "terrarium"
harness = "claude"
prompt = "project-lead"
roles = ["project_admin"]
"#;

    // PLAN §9, a project.
    const PROJECT: &str = r#"
# projects/terrarium.toml
name = "terrarium"
description = "The Terrarium tool itself"

[[repos]]
name = "terrarium"
url = "git@github.com:wiltaylor/terrarium.git"
default_branch = "main"
credential = "github-deploy"          # key in /state/credentials, never given to agents

[[repos]]
name = "copilot-prompt"
url = "https://github.com/wiltaylor/CopilotCustomSystemPrompt.git"

[checkout]
on_agent_create = ["terrarium"]       # cloned into $HOME/work/terrarium when the agent is made
"#;

    // PLAN §19, a target.
    const TARGET: &str = r#"
# targets/lab-win11.toml
kind = "vmlab"
host = "lab1"                 # host agent name
lab  = "/home/alice/labs/win11.wcl"
machine = "win11"
ephemeral = true
snapshot_before_task = true
"#;

    // PLAN §16, a subscription.
    const SUBSCRIPTION: &str = r#"
# subscriptions/anthropic-max.toml
name = "anthropic-max"
provider = "anthropic"
plan = "Max 20x"
harnesses = ["claude", "pi"]           # which harnesses can spend it
probe = "anthropic-oauth-usage"        # how to read remaining quota
alert_at = 80                          # percent of any window
"#;

    #[test]
    fn agent_policy_example_round_trips() {
        let def: AgentDef = round_trip(AGENT_POLICY);
        assert_eq!(def.harness, HarnessId::Pi);
        assert_eq!(def.subscription.as_deref(), Some("anthropic-max"));
        assert_eq!(def.policy.targets, ["buildbox", "lab-win11"]);
        assert_eq!(def.policy.mail_to, ["*"]);
        assert!(def.policy.cross_project.is_empty());
        assert_eq!(def.restart, RestartPolicy::OnFailure);
    }

    #[test]
    fn agent_lead_example_round_trips() {
        let def: AgentDef = round_trip(AGENT_LEAD);
        assert_eq!(def.name, "lead");
        assert_eq!(def.roles, [Role::ProjectAdmin]);
        assert_eq!(def.prompt.as_deref(), Some("project-lead"));
        assert_eq!(def.policy, Policy::default());
    }

    #[test]
    fn project_example_round_trips() {
        let def: ProjectDef = round_trip(PROJECT);
        assert_eq!(def.name, "terrarium");
        assert_eq!(def.repos.len(), 2);
        assert_eq!(def.repos[0].credential.as_deref(), Some("github-deploy"));
        assert_eq!(def.repos[1].default_branch, None);
        assert_eq!(def.checkout.on_agent_create, ["terrarium"]);
    }

    /// R3 of TER-1113 (TER-1199): a project file cannot give `board_flow =
    /// columns`; `code` and `rules` still read, and the stored word reads.
    #[test]
    fn a_project_file_cannot_give_the_columns_flow() {
        for (word, flow) in [("code", BoardFlow::Code), ("rules", BoardFlow::Rules)] {
            let def: ProjectDef =
                toml::from_str(&format!("name = \"p\"\nboard_flow = \"{word}\"\n")).expect(word);
            assert_eq!(def.board_flow, Some(flow));
        }
        let err = toml::from_str::<ProjectDef>("name = \"p\"\nboard_flow = \"columns\"\n")
            .expect_err("columns is not a file value");
        assert!(err.to_string().contains("columns"), "{err}");
        assert!(serde_json::from_value::<BoardFlow>(serde_json::json!("columns")).is_err());
        assert_eq!(BoardFlow::parse(Some("columns")), BoardFlow::Columns);
        assert_eq!(BoardFlow::Columns.as_str(), "columns");
    }

    #[test]
    fn target_example_round_trips() {
        let def: TargetDef = round_trip(TARGET);
        assert_eq!(def.kind, TargetKind::Vmlab);
        assert_eq!(def.host.as_deref(), Some("lab1"));
        assert!(def.ephemeral && def.snapshot_before_task && !def.keep);
    }

    #[test]
    fn subscription_example_round_trips() {
        let def: SubscriptionDef = round_trip(SUBSCRIPTION);
        assert_eq!(def.harnesses, [HarnessId::Claude, HarnessId::Pi]);
        assert_eq!(def.alert_at, 80);
    }

    // PLAN §8.6, an agent with skills.
    const AGENT_SKILLS: &str = r#"
name = "reviewer"
project = "terra"
harness = "claude"
skills = ["code-review", "release-notes"]
"#;

    #[test]
    fn agent_skills_example_round_trips() {
        let def: AgentDef = round_trip(AGENT_SKILLS);
        assert_eq!(def.skills, ["code-review", "release-notes"]);
        // No skills: the key is not written.
        let none: AgentDef = toml::from_str("project = \"p\"\nharness = \"pi\"\n").expect("parse");
        assert!(none.skills.is_empty());
        assert!(!toml::to_string(&none).expect("ser").contains("skills"));
    }

    #[test]
    fn auto_compact_keys_round_trip() {
        let def: AgentDef = toml::from_str(
            "project = \"p\"\nharness = \"claude\"\ncompact_on_idle = true\ncompact_max_age = \"24h\"\n",
        )
        .expect("parse");
        assert_eq!(def.compact_on_idle, Some(true));
        assert_eq!(
            def.compact_max_age,
            Some(CompactMaxAge::After(std::time::Duration::from_secs(86_400)))
        );
        let text = toml::to_string(&def).expect("ser");
        assert!(text.contains("compact_max_age = \"1d\""), "{text}");
        assert_eq!("off".parse::<CompactMaxAge>(), Ok(CompactMaxAge::Off));
        assert_eq!("0".parse::<CompactMaxAge>(), Ok(CompactMaxAge::Off));
        let bad = "soon".parse::<CompactMaxAge>().unwrap_err();
        assert!(bad.contains("compact max age"), "{bad}");
        // Absent keys stay absent: the global defaults apply.
        let none: AgentDef = toml::from_str("project = \"p\"\nharness = \"pi\"\n").expect("parse");
        assert_eq!((none.compact_on_idle, none.compact_max_age), (None, None));
        assert!(!toml::to_string(&none).expect("ser").contains("compact"));
    }

    #[test]
    fn restart_policy_uses_kebab_case() {
        let def: AgentDef =
            toml::from_str("project = \"p\"\nharness = \"codex\"\nrestart = \"always\"\n")
                .expect("parse");
        assert_eq!(def.restart, RestartPolicy::Always);
        assert!(
            toml::to_string(&AgentDef {
                restart: RestartPolicy::OnFailure,
                idle_stop: None,
                ..def
            })
            .expect("serialise")
            .contains("restart = \"on-failure\"")
        );
    }

    #[test]
    fn load_reads_a_config_dir_and_names_from_stems() {
        let dir = std::env::temp_dir().join(format!("terrarium-core-test-{}", ulid::Ulid::new()));
        for sub in ["projects", "agents", "targets", "subscriptions"] {
            std::fs::create_dir_all(dir.join(sub)).expect("mkdir");
        }
        std::fs::write(dir.join("projects/terrarium.toml"), PROJECT).expect("write");
        std::fs::write(dir.join("agents/worker.toml"), AGENT_POLICY).expect("write");
        std::fs::write(dir.join("agents/terrarium-lead.toml"), AGENT_LEAD).expect("write");
        std::fs::write(dir.join("agents/broken.toml"), "harness = 7").expect("write");
        std::fs::write(dir.join("agents/notes.txt"), "ignored").expect("write");
        std::fs::write(dir.join("targets/lab-win11.toml"), TARGET).expect("write");
        // subscriptions/ stays empty.

        let (defs, errors) = Definitions::load(&dir);
        std::fs::remove_dir_all(&dir).expect("cleanup");

        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(defs.projects.len(), 1);
        let names: Vec<_> = defs.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["lead", "worker"]);
        assert_eq!(defs.targets[0].name, "lab-win11");
        assert!(defs.subscriptions.is_empty());
    }

    // PLAN §8, an agent with its own base system prompt.
    const AGENT_SYSTEM_PROMPT: &str = r#"
project = "terrarium"
harness = "codex"
prompt = "default"
system_prompt = """
You are a careful reviewer for {{project.name}}.
"""
"#;

    #[test]
    fn agent_system_prompt_example_round_trips() {
        let def: AgentDef = round_trip(AGENT_SYSTEM_PROMPT);
        assert_eq!(
            def.system_prompt.as_deref(),
            Some("You are a careful reviewer for {{project.name}}.\n")
        );
        let none: AgentDef = round_trip(AGENT_LEAD);
        assert_eq!(none.system_prompt, None);
        assert!(!toml::to_string(&none).unwrap().contains("system_prompt"));
    }

    #[test]
    fn system_prompt_file_is_relative_to_the_definition_file() {
        let dir = std::env::temp_dir().join(format!("terrarium-core-sp-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("agents")).expect("mkdir");
        std::fs::create_dir_all(dir.join("projects")).expect("mkdir");
        std::fs::write(dir.join("agents/lead.md"), "Lead {{agent.name}}.\n").expect("write");
        std::fs::write(
            dir.join("agents/lead.toml"),
            "project = \"p\"\nharness = \"pi\"\nsystem_prompt_file = \"lead.md\"\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("agents/both.toml"),
            "project = \"p\"\nharness = \"pi\"\nsystem_prompt = \"x\"\nsystem_prompt_file = \"lead.md\"\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("agents/gone.toml"),
            "project = \"p\"\nharness = \"pi\"\nsystem_prompt_file = \"nope.md\"\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("projects/p.toml"),
            "[[templates]]\nname = \"w\"\nharness = \"pi\"\nsystem_prompt_file = \"../agents/lead.md\"\n",
        )
        .expect("write");
        let (defs, errors) = Definitions::load(&dir);
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(errors.len(), 2, "{errors:?}");
        let all = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("not both"), "{all}");
        assert!(all.contains("nope.md"), "{all}");
        assert_eq!(defs.agents.len(), 1);
        assert_eq!(
            defs.agents[0].system_prompt.as_deref(),
            Some("Lead {{agent.name}}.\n")
        );
        assert_eq!(
            defs.projects[0].templates[0].spec["system_prompt"],
            "Lead {{agent.name}}.\n"
        );
    }

    #[test]
    fn system_prompt_limit_and_empty_text() {
        assert_eq!(clean_system_prompt(None), Ok(None));
        assert_eq!(clean_system_prompt(Some(" \n".into())), Ok(None));
        let max = "x".repeat(SYSTEM_PROMPT_MAX);
        assert_eq!(clean_system_prompt(Some(max.clone())), Ok(Some(max)));
        let e = clean_system_prompt(Some("x".repeat(SYSTEM_PROMPT_MAX + 1))).unwrap_err();
        assert!(e.contains("32768"), "{e}");
        assert!(e.contains(ENV_SYSTEM_PROMPT_MAX), "{e}");
    }

    #[test]
    fn repo_hooks_dir_and_push_deny() {
        for ok in [".githooks", "tools/hooks", "a.b/c_d"] {
            assert_eq!(check_hooks_dir(ok), Ok(()), "{ok}");
        }
        for bad in [
            "", "/abs", "x/", "a//b", "./x", "a/../b", ".git", "x/.GIT/h", "a\\b",
        ] {
            assert!(check_hooks_dir(bad).is_err(), "{bad}");
        }
        for ok in ["op/*", "main", "release/v?.*", "user@x+y"] {
            assert_eq!(check_push_deny_glob(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "refs/heads/x",
            "a b",
            "x;rm",
            "$(x)",
            "a|b",
            "[ab]",
            "a)",
        ] {
            assert!(check_push_deny_glob(bad).is_err(), "{bad}");
        }
        assert!(check_push_deny(&vec!["x".to_owned(); PUSH_DENY_MAX]).is_ok());
        assert!(check_push_deny(&vec!["x".to_owned(); PUSH_DENY_MAX + 1]).is_err());
        for (glob, branch, hit) in [
            ("op/*", "op/x", true),
            ("op/*", "op/a/b", true),
            ("op/*", "op/", true),
            ("op/*", "op", false),
            ("op/*", "dev/op/x", false),
            ("main", "main", true),
            ("main", "main2", false),
            ("main", "mai", false),
            ("rel-?", "rel-1", true),
            ("rel-?", "rel-10", false),
            ("*", "anything/at/all", true),
            ("*-wip", "a-b-wip", true),
            ("*-wip", "a-b-wipx", false),
            ("a*b*c", "aXbYbZc", true),
            ("a*b*c", "aXbYcZ", false),
            ("**", "", true),
        ] {
            assert_eq!(push_deny_matches(glob, branch), hit, "{glob} {branch}");
        }
        let list = vec!["main".to_owned(), "op/*".to_owned()];
        assert_eq!(push_deny_match(&list, "op/1"), Some("op/*"));
        assert_eq!(push_deny_match(&list, "dev"), None);
        assert_eq!(PushVia::parse("proxy"), Ok(PushVia::Proxy));
        assert!(PushVia::parse("ssh").is_err());
        let def: RepoDef = toml::from_str(
            "name = \"app\"\nurl = \"https://x/app\"\nhooks_dir = \".githooks\"\npush_deny = [\"op/*\"]\n",
        )
        .unwrap();
        assert_eq!(def.hooks_dir.as_deref(), Some(".githooks"));
        assert_eq!(def.push_deny, ["op/*"]);
        assert_eq!(def.push_via, None);
        let def: RepoDef =
            toml::from_str("name = \"app\"\nurl = \"https://x/app\"\npush_via = \"direct\"\n")
                .unwrap();
        assert_eq!(def.push_via, Some(PushVia::Direct));
        let back = toml::to_string(&def).unwrap();
        assert!(back.contains("push_via = \"direct\""), "{back}");
        assert!(
            toml::from_str::<RepoDef>("name = \"a\"\nurl = \"u\"\npush_via = \"ssh\"\n").is_err()
        );
    }

    #[test]
    fn system_prompt_max_from_the_env_value() {
        assert_eq!(parse_system_prompt_max(None), Ok(SYSTEM_PROMPT_MAX));
        assert_eq!(parse_system_prompt_max(Some(" ")), Ok(SYSTEM_PROMPT_MAX));
        assert_eq!(parse_system_prompt_max(Some("65536")), Ok(64 * 1024));
        assert_eq!(parse_system_prompt_max(Some("64K")), Ok(64 * 1024));
        assert_eq!(parse_system_prompt_max(Some("16KiB")), Ok(16 * 1024));
        assert_eq!(
            parse_system_prompt_max(Some("96k")),
            Ok(SYSTEM_PROMPT_MAX_CEILING)
        );
        for bad in ["0", "97K", "1M", "lots", "-5"] {
            assert!(parse_system_prompt_max(Some(bad)).is_err(), "{bad}");
        }
        assert!(system_prompt_too_large("x", system_prompt_max()).is_none());
        let e = system_prompt_too_large("the persona `big`", system_prompt_max() + 1).unwrap();
        assert!(e.starts_with("the persona `big` has "), "{e}");
    }

    #[test]
    fn load_of_a_missing_dir_is_empty() {
        let (defs, errors) = Definitions::load(Path::new("/nonexistent/terrarium"));
        assert!(errors.is_empty());
        assert_eq!(defs, Definitions::default());
    }

    #[test]
    fn spawn_policy_checks_and_matches_templates() {
        let sp: SpawnPolicy =
            serde_json::from_str(r#"{"templates": ["worker", "global:helper", "project:*"]}"#)
                .unwrap();
        assert_eq!(sp.max_concurrent, SPAWN_DEFAULT_CONCURRENT);
        assert_eq!(sp.lifetime().unwrap(), SPAWN_DEFAULT_LIFETIME);
        sp.check().unwrap();
        let t = |name, global| SpawnTemplate { name, global };
        assert!(sp.allows(t("worker", false)));
        assert!(sp.allows(t("worker", true)));
        assert!(sp.allows(t("helper", true)));
        assert!(sp.allows(t("other", false)));
        assert!(!sp.allows(t("other", true)));
        let only_global = SpawnPolicy {
            templates: vec!["global:*".into()],
            ..SpawnPolicy::default()
        };
        assert!(only_global.allows(t("x", true)));
        assert!(!only_global.allows(t("x", false)));
        assert!(!SpawnPolicy::default().allows(t("x", false)));
        for (json, why) in [
            (r#"{"templates": ["a b"]}"#, "spawn.templates"),
            (r#"{"max_concurrent": 0}"#, "max_concurrent"),
            (r#"{"max_per_day": 0}"#, "max_per_day"),
            (r#"{"max_lifetime": "25h"}"#, "24h"),
            (r#"{"max_lifetime": "0s"}"#, "max_lifetime"),
            (r#"{"max_bound_lifetime": "31d"}"#, "30d"),
            (r#"{"max_bound_lifetime": "x"}"#, "max_bound_lifetime"),
        ] {
            let sp: SpawnPolicy = serde_json::from_str(json).unwrap();
            let e = sp.check().unwrap_err();
            assert!(e.contains(why), "{json}: {e}");
        }
        assert!(serde_json::from_str::<SpawnPolicy>(r#"{"bogus": 1}"#).is_err());
        let p = SpawnPolicy {
            may_spawn_children: true,
            max_lifetime: Some("30m".into()),
            ..SpawnPolicy::default()
        };
        assert_eq!(p.lifetime().unwrap().as_secs(), 1800);
        assert!(!p.for_child().may_spawn_children);
        // Bound agents (TER-74): off by default, 30 days by default.
        assert!(!p.bound);
        assert_eq!(p.bound_lifetime().unwrap(), SPAWN_MAX_BOUND_LIFETIME);
        let b: SpawnPolicy =
            serde_json::from_str(r#"{"bound": true, "max_bound_lifetime": "3d"}"#).unwrap();
        b.check().unwrap();
        assert_eq!(b.bound_lifetime().unwrap().as_secs(), 3 * 24 * 3600);
        assert!(!b.for_child().bound);
        // A policy without the block reads and writes as before.
        let pol: Policy = serde_json::from_str(r#"{"targets": []}"#).unwrap();
        assert!(pol.spawn.is_none());
        assert!(!serde_json::to_string(&pol).unwrap().contains("spawn"));
    }
}
