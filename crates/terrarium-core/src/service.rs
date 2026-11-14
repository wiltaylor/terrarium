//! Services (TER-1264, Services C1, design D1-D3): a named object with a
//! request queue. A caller sends a request (JSON and an optional payload
//! file) and gets a request ID at once; the daemon runs the request later
//! on the service's target (C2-C5).
//!
//! A service is defined in `services/<name>.toml` in the config repo or by
//! `POST /api/services`, as tool proxies are. This module has the
//! definition, the request states, and the checks that need no store
//! ([`ServiceDef::check`]). The daemon adds the checks against its store:
//! the project, the target and its kind, and the callers.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The longest service name.
pub const NAME_MAX: usize = 32;
/// The default and the bounds of `max_run_time`.
pub const DEFAULT_MAX_RUN_TIME: Duration = Duration::from_secs(3600);
pub const MAX_RUN_TIME_MAX: Duration = Duration::from_secs(24 * 3600);
/// The default and the bounds of `wait_limit` (design D3, Q5 A).
pub const DEFAULT_WAIT_LIMIT: Duration = Duration::from_secs(24 * 3600);
pub const WAIT_LIMIT_MAX: Duration = Duration::from_secs(7 * 24 * 3600);
/// The shortest `max_run_time` and `wait_limit`.
pub const LIMIT_MIN: Duration = Duration::from_secs(60);
/// The default and the most of `concurrency` (Q3 A).
pub const DEFAULT_CONCURRENCY: u32 = 1;
pub const CONCURRENCY_MAX: u32 = 16;
/// The payload limits (Q2 A, O2 of TER-1264): per payload and retained
/// per service. Both are per-service settings.
pub const DEFAULT_MAX_PAYLOAD: u64 = 64 * 1024 * 1024;
pub const MAX_PAYLOAD_MAX: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_MAX_RETAINED: u64 = 1024 * 1024 * 1024;
/// The most that one service may keep: a tenth of the global cap, so one
/// project's service cannot take the store of all others (TER-1264 review
/// finding 5).
pub const MAX_RETAINED_MAX: u64 = 10 * 1024 * 1024 * 1024;
/// The global payload cap and the free-space floor of the state
/// filesystem (O2). Not per service.
pub const GLOBAL_PAYLOAD_CAP: u64 = 100 * 1024 * 1024 * 1024;
pub const MIN_FREE_PERCENT: u64 = 20;
/// How long the daemon keeps a payload (Q2 A).
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// The largest request JSON.
pub const REQUEST_JSON_MAX: usize = 256 * 1024;

/// The most `env_secrets` of one script service.
pub const ENV_SECRETS_MAX: usize = 32;

/// What runs a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A command on the target (C3).
    Script,
    /// An agent from a template (C4).
    Agent,
}

/// The resources that one request claims while it runs (design D1). C2
/// turns them into a queued reservation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// CPU in millicores (1000 is one core).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cpu_millis: u64,
    /// A size such as `8GiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// How many GPUs of the target's host.
    #[serde(default, skip_serializing_if = "is_zero32")]
    pub gpus: u32,
    /// The GPUs are the request's alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gpu_exclusive: bool,
    /// The VRAM share of each shared GPU, such as `16GiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_vram: Option<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero32(n: &u32) -> bool {
    *n == 0
}

impl Resources {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// A service definition. `deny_unknown_fields`: a typo is an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub kind: Kind,
    /// The target that runs the requests.
    pub target: String,
    /// `script`: the program and its fixed arguments (C3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// `script`: env var name to secret name (C3). The daemon sets each
    /// value in the environment of the run on the target, never on a
    /// command line or in a log.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_secrets: BTreeMap<String, String>,
    /// `agent`: the template of the agent (C4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// `agent`: the prompt; the request JSON is added to it (C4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Resources::is_empty")]
    pub resources: Resources,
    /// A duration such as `2h` (default 1h, 1m to 24h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_time: Option<String>,
    /// Requests that run or wait for resources at one time (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    /// How long a request may wait to start (default 24h, 1m to 7d).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_limit: Option<String>,
    /// The largest payload of one request (default 64MiB, at most 1GiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_payload: Option<String>,
    /// The payload bytes this service keeps at one time (default 1GiB, at
    /// most 10GiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retained: Option<String>,
    /// Who may call: `project:<name>` or `agent:<project>/<name>`. Empty:
    /// the service's own project (a global service: only its admins).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<String>,
    /// The owning project. A service without one is global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `8388608`, `512K`, `64MiB`, `1G`, `2GiB` (any case) as bytes.
pub fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim();
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (n, unit) = v.split_at(split);
    let n: u64 = n.parse().ok()?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(mult)
}

fn duration(field: &str, v: &str, min: Duration, max: Duration) -> Result<Duration, String> {
    let d = crate::eta::parse_duration(v).map_err(|e| format!("{field}: {e}"))?;
    let secs = u64::try_from(d.num_seconds()).unwrap_or(0);
    let d = Duration::from_secs(secs);
    if d < min || d > max {
        return Err(format!(
            "{field} `{v}` must be {} to {}",
            human(min),
            human(max)
        ));
    }
    Ok(d)
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s.is_multiple_of(86_400) {
        format!("{}d", s / 86_400)
    } else if s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else {
        format!("{}m", s / 60)
    }
}

fn size(field: &str, v: &str) -> Result<u64, String> {
    parse_size(v)
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("bad {field} `{v}`: use a size such as 64MiB or 1GiB"))
}

/// A valid service name: 1-32 of `a-z`, `0-9`, `-`, starting with a letter.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = name.len() <= NAME_MAX
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        return Err(format!(
            "bad service name `{name}`: use 1-{NAME_MAX} of a-z, 0-9, and `-`, starting with a letter"
        ));
    }
    Ok(())
}

/// A caller entry: `project:<name>` or `agent:<project>/<name>`.
pub fn check_caller(c: &str) -> Result<(), String> {
    let word = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    let ok = match c.split_once(':') {
        Some(("project", p)) => word(p),
        Some(("agent", rest)) => rest
            .split_once('/')
            .is_some_and(|(p, n)| word(p) && word(n)),
        _ => false,
    };
    if !ok {
        return Err(format!(
            "bad caller `{c}`: use project:<name> or agent:<project>/<name>"
        ));
    }
    Ok(())
}

impl ServiceDef {
    /// Parse the text of a `services/<name>.toml` file. The name comes
    /// from the file name, not from the text.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// The checks that need no store.
    pub fn check(&self) -> Result<(), String> {
        check_name(&self.name)?;
        if self.target.trim().is_empty() {
            return Err("`target` is empty".into());
        }
        match self.kind {
            Kind::Script => {
                if self.command.is_empty() || self.command[0].trim().is_empty() {
                    return Err("a script service needs `command`: a non-empty argv such as [\"/opt/svc/run\"]".into());
                }
                if self.command.iter().any(|w| w.contains('\0')) {
                    return Err("`command` has a NUL byte".into());
                }
                if self.template.is_some() || self.prompt.is_some() {
                    return Err("`template` and `prompt` are for an agent service".into());
                }
                if self.env_secrets.len() > ENV_SECRETS_MAX {
                    return Err(format!("env_secrets has more than {ENV_SECRETS_MAX} entries"));
                }
                for (env, secret) in &self.env_secrets {
                    crate::tool::check_env(env)?;
                    if secret.trim().is_empty() {
                        return Err(format!("env_secrets `{env}` names no secret"));
                    }
                }
            }
            Kind::Agent => {
                if self.template.as_deref().is_none_or(|t| t.trim().is_empty()) {
                    return Err("an agent service needs `template`".into());
                }
                if self.prompt.as_deref().is_none_or(|p| p.trim().is_empty()) {
                    return Err("an agent service needs `prompt`".into());
                }
                if !self.command.is_empty() {
                    return Err("`command` is for a script service".into());
                }
                if !self.env_secrets.is_empty() {
                    return Err("`env_secrets` is for a script service".into());
                }
            }
        }
        let r = &self.resources;
        if let Some(m) = &r.memory {
            size("resources.memory", m)?;
        }
        if r.gpu_exclusive && r.gpus == 0 {
            return Err("resources.gpu_exclusive needs resources.gpus of 1 or more".into());
        }
        if let Some(v) = &r.gpu_vram {
            if r.gpus == 0 {
                return Err("resources.gpu_vram needs resources.gpus of 1 or more".into());
            }
            if r.gpu_exclusive {
                return Err("resources.gpu_vram is for a shared GPU; an exclusive GPU takes the whole device".into());
            }
            size("resources.gpu_vram", v)?;
        } else if r.gpus > 0 && !r.gpu_exclusive {
            return Err(
                "a shared GPU needs resources.gpu_vram (its share), or set resources.gpu_exclusive"
                    .into(),
            );
        }
        if let Some(v) = &self.max_run_time {
            duration("max_run_time", v, LIMIT_MIN, MAX_RUN_TIME_MAX)?;
        }
        if let Some(v) = &self.wait_limit {
            duration("wait_limit", v, LIMIT_MIN, WAIT_LIMIT_MAX)?;
        }
        if let Some(c) = self.concurrency
            && !(1..=CONCURRENCY_MAX).contains(&c)
        {
            return Err(format!("concurrency must be 1 to {CONCURRENCY_MAX}"));
        }
        if let Some(v) = &self.max_payload
            && size("max_payload", v)? > MAX_PAYLOAD_MAX
        {
            return Err(format!("max_payload `{v}` is over 1GiB"));
        }
        if let Some(v) = &self.max_retained
            && size("max_retained", v)? > MAX_RETAINED_MAX
        {
            return Err(format!("max_retained `{v}` is over 10GiB"));
        }
        if self.max_retained_bytes() < self.max_payload_bytes() {
            return Err("max_retained is smaller than max_payload".into());
        }
        for c in &self.callers {
            check_caller(c)?;
        }
        Ok(())
    }

    /// `None` when the service is global.
    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn max_run_time(&self) -> Duration {
        self.max_run_time
            .as_deref()
            .and_then(|v| duration("", v, LIMIT_MIN, MAX_RUN_TIME_MAX).ok())
            .unwrap_or(DEFAULT_MAX_RUN_TIME)
    }

    pub fn wait_limit(&self) -> Duration {
        self.wait_limit
            .as_deref()
            .and_then(|v| duration("", v, LIMIT_MIN, WAIT_LIMIT_MAX).ok())
            .unwrap_or(DEFAULT_WAIT_LIMIT)
    }

    pub fn concurrency(&self) -> u32 {
        self.concurrency.unwrap_or(DEFAULT_CONCURRENCY)
    }

    pub fn max_payload_bytes(&self) -> u64 {
        self.max_payload
            .as_deref()
            .and_then(parse_size)
            .unwrap_or(DEFAULT_MAX_PAYLOAD)
    }

    pub fn max_retained_bytes(&self) -> u64 {
        self.max_retained
            .as_deref()
            .and_then(parse_size)
            .unwrap_or(DEFAULT_MAX_RETAINED)
    }
}

/// The state of a request (design D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Accepted; behind the service's concurrency limit.
    Queued,
    /// Waiting for its reservation (C2).
    Waiting,
    Running,
    Done,
    Failed,
    Cancelled,
    TimedOut,
}

impl State {
    pub const ALL: [State; 7] = [
        Self::Queued,
        Self::Waiting,
        Self::Running,
        Self::Done,
        Self::Failed,
        Self::Cancelled,
        Self::TimedOut,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|st| st.as_str() == s)
    }

    /// The request has ended.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    /// The request holds one of the service's concurrency places.
    pub fn holds_place(self) -> bool {
        matches!(self, Self::Waiting | Self::Running)
    }

    /// May a request go from `self` to `to`?
    pub fn can_go(self, to: Self) -> bool {
        use State::*;
        matches!(
            (self, to),
            (Queued, Waiting | Cancelled | TimedOut | Failed)
                | (Waiting, Running | Queued | Cancelled | TimedOut | Failed)
                | (Running, Done | Failed | Cancelled | TimedOut)
        )
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const SCRIPT: &str =
        "kind = \"script\"\ntarget = \"helios-docker\"\ncommand = [\"/opt/svc/run\"]\n";

    fn def(extra: &str) -> ServiceDef {
        let mut d: ServiceDef = toml::from_str(&format!("{SCRIPT}{extra}")).unwrap();
        d.name = "video-gen".into();
        d
    }

    #[test]
    fn a_full_definition_parses_and_checks() {
        let d = def(
            "description = \"Video\"\nmax_run_time = \"2h\"\nconcurrency = 2\nwait_limit = \"12h\"\n\
             max_payload = \"128MiB\"\nmax_retained = \"2GiB\"\ncallers = [\"project:terrarium\", \"agent:meta/lead\"]\n\
             project = \"terrarium\"\n[resources]\ncpu_millis = 4000\nmemory = \"16GiB\"\ngpus = 1\ngpu_exclusive = true\n",
        );
        d.check().unwrap();
        assert_eq!(d.max_run_time(), Duration::from_secs(7200));
        assert_eq!(d.wait_limit(), Duration::from_secs(12 * 3600));
        assert_eq!(d.concurrency(), 2);
        assert_eq!(d.max_payload_bytes(), 128 << 20);
        assert_eq!(d.max_retained_bytes(), 2 << 30);
    }

    #[test]
    fn the_defaults() {
        let d = def("");
        d.check().unwrap();
        assert_eq!(d.max_run_time(), DEFAULT_MAX_RUN_TIME);
        assert_eq!(d.wait_limit(), Duration::from_secs(24 * 3600));
        assert_eq!(d.concurrency(), 1);
        assert_eq!(d.max_payload_bytes(), 64 << 20);
        assert_eq!(d.max_retained_bytes(), 1 << 30);
    }

    #[test]
    fn unknown_fields_and_kinds_are_refused() {
        for text in [
            format!("{SCRIPT}shell = true\n"),
            format!("{SCRIPT}[resources]\ngpu = 1\n"),
            "kind = \"lambda\"\ntarget = \"t\"\n".to_owned(),
        ] {
            assert!(toml::from_str::<ServiceDef>(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_kind_needs_its_fields() {
        let mut d = def("");
        d.command.clear();
        assert!(d.check().unwrap_err().contains("needs `command`"));
        let mut a: ServiceDef =
            toml::from_str("kind = \"agent\"\ntarget = \"t\"\ntemplate = \"svc\"\n").unwrap();
        a.name = "a".into();
        assert!(a.check().unwrap_err().contains("needs `prompt`"));
        a.prompt = Some("Make a video".into());
        a.check().unwrap();
        a.command = vec!["/bin/x".into()];
        assert!(a.check().unwrap_err().contains("for a script"));
        let mut s = def("");
        s.template = Some("t".into());
        assert!(s.check().unwrap_err().contains("for an agent"));
        a.command.clear();
        a.env_secrets.insert("TOKEN".into(), "t".into());
        assert!(a.check().unwrap_err().contains("for a script"));
    }

    #[test]
    fn env_secrets_of_a_script() {
        let d = def("env_secrets = { HF_TOKEN = \"hf-token\" }\n");
        d.check().unwrap();
        assert_eq!(d.env_secrets["HF_TOKEN"], "hf-token");
        for (extra, says) in [
            ("env_secrets = { token = \"t\" }\n", "bad env name"),
            ("env_secrets = { PATH = \"t\" }\n", "may not set"),
            ("env_secrets = { TERRARIUM_SERVICE_DIR = \"t\" }\n", "may not set"),
            ("env_secrets = { TOKEN = \" \" }\n", "names no secret"),
        ] {
            let e = def(extra).check().unwrap_err();
            assert!(e.contains(says), "{extra}: {e}");
        }
        let mut many = def("");
        for i in 0..=ENV_SECRETS_MAX {
            many.env_secrets.insert(format!("S{i}"), "x".into());
        }
        assert!(many.check().unwrap_err().contains("more than"));
    }

    #[test]
    fn the_limits() {
        for (extra, says) in [
            ("max_run_time = \"30s\"\n", "max_run_time"),
            ("max_run_time = \"25h\"\n", "max_run_time"),
            ("wait_limit = \"8d\"\n", "wait_limit"),
            ("wait_limit = \"soon\"\n", "wait_limit"),
            ("concurrency = 0\n", "concurrency"),
            ("concurrency = 17\n", "concurrency"),
            ("max_payload = \"2GiB\"\n", "over 1GiB"),
            ("max_payload = \"lots\"\n", "bad max_payload"),
            ("max_retained = \"11GiB\"\n", "over 10GiB"),
            (
                "max_payload = \"512MiB\"\nmax_retained = \"256MiB\"\n",
                "smaller",
            ),
            ("callers = [\"everyone\"]\n", "bad caller"),
            ("callers = [\"agent:lead\"]\n", "bad caller"),
            ("[resources]\ngpu_exclusive = true\n", "gpus of 1"),
            ("[resources]\ngpus = 1\n", "gpu_vram"),
            (
                "[resources]\ngpus = 1\ngpu_exclusive = true\ngpu_vram = \"8GiB\"\n",
                "shared GPU",
            ),
            ("[resources]\nmemory = \"lots\"\n", "resources.memory"),
        ] {
            let e = def(extra).check().unwrap_err();
            assert!(e.contains(says), "{extra}: {e}");
        }
        def("[resources]\ngpus = 2\ngpu_vram = \"8GiB\"\n")
            .check()
            .unwrap();
    }

    #[test]
    fn the_name_rules() {
        for good in ["a", "video-gen", &"a".repeat(NAME_MAX)] {
            check_name(good).unwrap();
        }
        for bad in ["", "Video", "1a", "a_b", "a/b", &"a".repeat(NAME_MAX + 1)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("64MiB"), Some(64 << 20));
        assert_eq!(parse_size("1g"), Some(1 << 30));
        assert_eq!(parse_size("512K"), Some(512 << 10));
        assert_eq!(parse_size("10"), Some(10));
        assert_eq!(parse_size("1T"), None);
    }

    #[test]
    fn the_state_edges() {
        use State::*;
        assert!(Queued.can_go(Waiting));
        assert!(Queued.can_go(TimedOut));
        assert!(Waiting.can_go(Running));
        assert!(Waiting.can_go(TimedOut));
        assert!(Running.can_go(Done));
        assert!(Running.can_go(Cancelled));
        assert!(!Queued.can_go(Running));
        assert!(!Queued.can_go(Done));
        for end in [Done, Failed, Cancelled, TimedOut] {
            assert!(end.is_final());
            for to in State::ALL {
                assert!(!end.can_go(to), "{end} -> {to}");
            }
        }
        for s in State::ALL {
            assert_eq!(State::parse(s.as_str()), Some(s));
        }
        assert_eq!(serde_json::to_string(&TimedOut).unwrap(), "\"timed_out\"");
    }
}
