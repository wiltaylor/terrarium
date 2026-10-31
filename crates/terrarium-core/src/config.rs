//! On-disk TOML definitions: projects, agents, targets, and subscriptions.
//!
//! The daemon reads these from `$TERRARIUM_CONFIG_DIR` (default
//! `/etc/terrarium`), one file per object, in the `projects/`, `agents/`,
//! `targets/`, and `subscriptions/` directories. When a file has no `name`,
//! the file stem is the name.

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
    /// The skills of the agent from the skill library (PLAN §8.6): skill
    /// names, or `*` for every skill of the library. Default none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Start a fresh session, not a resume, when the rendered prompt
    /// changed and the harness keeps the prompt of a session
    /// (`Adapter::prompt_frozen_on_resume`, for example Codex). The old
    /// session stays in the history. Default true.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub new_session_on_prompt_change: bool,
}

fn yes() -> bool {
    true
}

/// The most bytes in an agent's `system_prompt`.
pub const SYSTEM_PROMPT_MAX: usize = 32 * 1024;

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

/// The key in a definition file that names a file with the system prompt.
pub const SYSTEM_PROMPT_FILE_KEY: &str = "system_prompt_file";

/// Is `text` a system prompt that the agent may have? An empty text is
/// valid: it means "no system prompt".
pub fn check_system_prompt(text: &str) -> std::result::Result<(), String> {
    if text.len() > SYSTEM_PROMPT_MAX {
        return Err(format!(
            "`system_prompt` has {} bytes; the limit is {SYSTEM_PROMPT_MAX}",
            text.len()
        ));
    }
    Ok(())
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckoutDef {
    /// Repos cloned into `$HOME/work/<repo>` when an agent is made.
    #[serde(default)]
    pub on_agent_create: Vec<String>,
}

/// A project definition, `projects/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDef {
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
named!(TargetDef, SubscriptionDef);

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
        };
        (defs, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
lab  = "/home/wil/labs/win11.wcl"
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
    }

    #[test]
    fn load_of_a_missing_dir_is_empty() {
        let (defs, errors) = Definitions::load(Path::new("/nonexistent/terrarium"));
        assert!(errors.is_empty());
        assert_eq!(defs, Definitions::default());
    }
}
