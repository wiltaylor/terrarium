//! Dashboard login flows, harness status checks, and stored API keys
//! (PLAN §16.1).
//!
//! A login session runs the harness's own login command in a PTY through
//! [`terrarium_runner::Runner`], as the daemon user, with no sandbox:
//!
//! | Kind | Command |
//! |---|---|
//! | `claude` | `claude auth login` (a URL, then a pasted code) |
//! | `codex` | `codex login --device-auth` (a URL and a device code) |
//! | `copilot` | `copilot login --device-code` (a URL and a device code) |
//!
//! pi uses API keys only (PLAN §16.3): [`start`] refuses the kind `pi`.
//! A pi API key goes through `POST /api/subscriptions/{name}/api-key`. The
//! kind `pi` stays for the status check of a pi subscription
//! (`pi auth check`).
//!
//! The command runs in a scratch home (`<temp dir>/terrarium-login-<id>/home`,
//! mode 0700), never in the shared home. When it succeeds, only the
//! credential moves from the scratch home into the account of the
//! subscription (`crate::accounts::import_login`, PLAN §16.3). A login with
//! no `name` whose default subscription has an account already makes a
//! second account (`anthropic-max-20x-2`). No other account and no shared
//! file changes. The scratch home goes with the temp dir.
//!
//! The session reads the raw PTY output, and per-kind patterns find the URL
//! and the device code (see [`parse_output`]; the fixtures are in
//! `tests/fixtures/login/`). The output stays in memory only: the runner of
//! a login writes no `pty.log` (`log_max_bytes = 0`), so the device code
//! never reaches the disk (`docs/security-review.md`, F6). When the command exits with code 0, the session runs the
//! status command, makes or updates the subscription definition, and runs
//! its probe once. The session ends then, or after the login timeout (ten
//! minutes), or on cancel; its temp dir (`$TMPDIR/terrarium-login-<id>`,
//! mode 0700, with the runner socket and `events.jsonl`) is deleted before
//! the final state is stored. A dir that a daemon crash left behind is
//! deleted at the next login start.
//!
//! Rules: only the operator starts, reads, types into, or attaches to a
//! login session. A pasted code goes into the PTY and is never stored or
//! sent back. Each step writes an audit row.
//!
//! Stored API keys live in `<state>/credentials/subscriptions/<name>.json`,
//! mode 0600. [`credential_env_for`] gives the env for an agent on the
//! subscription.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::config::SubscriptionDef;
use terrarium_core::{
    Adapter, Agent, AgentId, HarnessId, HarnessSessionId, HookPayload, LaunchSpec,
};
use terrarium_runner::{NoHandler, Runner, RunnerConfig};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::projects::SOURCE_API;
use crate::store::subscriptions::{self as subs, LoginRow, Subscription};
use crate::supervisor::{PtyAttachment, PtyInput};

/// The kinds of the Add-subscription wizard, and of the status checks.
/// `Pi` is a status check only: pi uses API keys only, so it has no login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginKind {
    Claude,
    Codex,
    Copilot,
    Pi,
}

impl LoginKind {
    pub fn as_str(self) -> &'static str {
        self.harness().as_str()
    }

    pub fn harness(self) -> HarnessId {
        match self {
            Self::Claude => HarnessId::Claude,
            Self::Codex => HarnessId::Codex,
            Self::Copilot => HarnessId::Copilot,
            Self::Pi => HarnessId::Pi,
        }
    }

    /// The arguments of the login command.
    pub fn login_args(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["auth", "login"],
            Self::Codex => &["login", "--device-auth"],
            Self::Copilot => &["login", "--device-code"],
            Self::Pi => &[],
        }
    }

    /// The arguments of the logout command, if the harness has one.
    pub fn logout_args(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Claude => Some(&["auth", "logout"]),
            Self::Codex => Some(&["logout"]),
            Self::Copilot => Some(&["logout"]),
            Self::Pi => None,
        }
    }

    /// The provider of a plan logged in with this kind.
    pub fn provider(self, pi_provider: Option<&str>) -> String {
        match self {
            Self::Claude => "anthropic".into(),
            Self::Codex => "openai".into(),
            Self::Copilot => "github".into(),
            Self::Pi => pi_provider.unwrap_or("anthropic").into(),
        }
    }

    /// What the operator does in the embedded terminal.
    pub fn instructions(self, provider: Option<&str>) -> String {
        match self {
            Self::Claude => "Open the URL, sign in, and paste the code that the page shows.".into(),
            Self::Codex | Self::Copilot => {
                "Open the URL, sign in, and enter the device code.".into()
            }
            Self::Pi => {
                let _ = provider;
                terrarium_adapter_pi::provider::PLAN_REFUSAL.into()
            }
        }
    }
}

/// What the patterns found in the login output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub url: Option<String>,
    pub device_code: Option<String>,
}

struct Patterns {
    osc8: Regex,
    ansi: Regex,
    claude_url: Regex,
    codex_url: Regex,
    codex_code: Regex,
    copilot: Regex,
    any_url: Regex,
    any_code: Regex,
}

fn patterns() -> Option<&'static Patterns> {
    static P: std::sync::OnceLock<Option<Patterns>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        Some(Patterns {
            // OSC 8 hyperlink targets: ESC ] 8 ; params ; URL BEL|ST.
            osc8: Regex::new(r"\x1b\]8;[^;\x07\x1b]*;([^\x07\x1b]+)(?:\x07|\x1b\\)").ok()?,
            // OSC strings, CSI sequences, and two-byte escapes.
            ansi: Regex::new(
                r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[@-Z\\-_]|\r",
            )
            .ok()?,
            claude_url: Regex::new(r"https://[^\s\x07\x1b]*/oauth/authorize\?[^\s\x07\x1b]+").ok()?,
            codex_url: Regex::new(r"https://auth\.openai\.com/codex/device[^\s\x07\x1b]*").ok()?,
            codex_code: Regex::new(r"(?s)one-time code.*?\b([A-Z0-9]{4}-[A-Z0-9]{4,6})\b").ok()?,
            copilot: Regex::new(
                r"visit\s+(https://[^\s]+/login/device)\s+and enter code\s+([A-Z0-9]{4}-[A-Z0-9]{4})",
            )
            .ok()?,
            any_url: Regex::new(r"https://[^\s\x07\x1b]+").ok()?,
            any_code: Regex::new(r"(?i)code[^A-Z0-9\n]{0,40}\b([A-Z0-9]{4}-[A-Z0-9]{4,6})\b").ok()?,
        })
    })
    .as_ref()
}

/// The text of the output with the escape sequences removed.
pub fn strip_ansi(raw: &str) -> String {
    match patterns() {
        Some(p) => p.ansi.replace_all(raw, "").into_owned(),
        None => raw.to_owned(),
    }
}

/// Find the login URL and the device code in the raw PTY output of a login
/// command. The last match wins, so a new code after a retry replaces the
/// old one.
pub fn parse_output(kind: LoginKind, raw: &[u8]) -> Parsed {
    let Some(p) = patterns() else {
        return Parsed::default();
    };
    let raw = String::from_utf8_lossy(raw);
    let links: Vec<String> = p
        .osc8
        .captures_iter(&raw)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_owned()))
        .collect();
    let text = strip_ansi(&raw);
    let last = |re: &Regex, s: &str, group: usize| -> Option<String> {
        re.captures_iter(s)
            .last()
            .and_then(|c| c.get(group).map(|m| m.as_str().to_owned()))
    };
    match kind {
        LoginKind::Claude => Parsed {
            url: links
                .iter()
                .rev()
                .find(|l| p.claude_url.is_match(l))
                .cloned()
                .or_else(|| last(&p.claude_url, &text, 0)),
            device_code: None,
        },
        LoginKind::Codex => Parsed {
            url: last(&p.codex_url, &text, 0),
            device_code: last(&p.codex_code, &text, 1),
        },
        LoginKind::Copilot => match p.copilot.captures_iter(&text).last() {
            Some(c) => Parsed {
                url: c.get(1).map(|m| m.as_str().to_owned()),
                device_code: c.get(2).map(|m| m.as_str().to_owned()),
            },
            None => Parsed {
                url: last(&p.any_url, &text, 0),
                device_code: last(&p.any_code, &text, 1),
            },
        },
        // pi has no login: it uses API keys only.
        LoginKind::Pi => Parsed::default(),
    }
}

/// A login check of one harness (and one pi provider).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginStatus {
    pub kind: LoginKind,
    pub harness: HarnessId,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_provider: Option<String>,
    pub logged_in: bool,
    /// The plan, when the status output says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// A short line from the status command, for the page. No tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Env var prefixes that would change which login a child process reads.
const STRIP_PREFIXES: [&str; 2] = ["CLAUDE", "ANTHROPIC"];
/// Also the daemon's own bearer token and the first operator password: a
/// login command needs neither.
const STRIP_VARS: [&str; 7] = [
    "COPILOT_GITHUB_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "OPENAI_API_KEY",
    "TERRARIUM_TOKEN",
    crate::password::ENV_PASSWORD,
    crate::password::ENV_PASSWORD_FILE,
];

/// The daemon's env without the variables that would pick another login,
/// plus the harness home.
fn child_env(kind: LoginKind, home: &Path) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| {
            !STRIP_PREFIXES.iter().any(|p| k.starts_with(p)) && !STRIP_VARS.contains(&k.as_str())
        })
        .collect();
    env.insert(
        kind.harness().home_env().to_owned(),
        home.display().to_string(),
    );
    env
}

/// The result of a short command. Both streams are kept: `codex login
/// status` prints its line to stderr and nothing to stdout.
struct CmdOut {
    code: i32,
    stdout: String,
    stderr: String,
}

impl CmdOut {
    /// The first non-empty line of stdout, else of stderr.
    fn first_line(&self) -> String {
        [&self.stdout, &self.stderr]
            .iter()
            .find_map(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or_default()
            .to_owned()
    }
}

/// Run a short command with the harness env.
async fn run_cmd(
    program: &str,
    args: &[&str],
    kind: LoginKind,
    home: &Path,
    extra: &[(String, String)],
) -> Result<CmdOut, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(child_env(kind, home))
        .envs(extra.iter().cloned())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| format!("`{program} {}` took longer than 30 s", args.join(" ")))?
        .map_err(|e| format!("cannot run `{program}`: {e}"))?;
    Ok(CmdOut {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// The plan by Anthropic's name of today
/// (`terrarium_core::providers::claude_plan_name`): `Ultra` from
/// `default_claude_max_20x` (the Max 20x plan is the Ultra plan since
/// 2026-10-30), `Max` from `default_claude_max_5x`, else the subscription
/// type.
pub fn claude_plan(subscription_type: Option<&str>, tier: Option<&str>) -> Option<String> {
    use terrarium_core::providers::claude_plan_name;
    if let Some(p) = tier.and_then(claude_plan_name) {
        return Some(p.into());
    }
    if let Some(p) = subscription_type.and_then(claude_plan_name) {
        return Some(p.into());
    }
    subscription_type.map(|s| {
        let mut c = s.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect())
            .unwrap_or_default()
    })
}

/// The ChatGPT plan from the `id_token` claims in Codex's `auth.json`. The
/// token is not verified or kept; only the plan claim leaves.
pub fn codex_plan(auth: &Value) -> Option<String> {
    use base64::Engine;
    let jwt = auth.get("tokens")?.get("id_token")?.as_str()?;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Which login a status check reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckHome {
    /// The shared harness home (a login from before the migration).
    Shared,
    /// The plan account of a subscription (PLAN §16.3).
    Account(String),
    /// The scratch home of a login session.
    Scratch(PathBuf),
}

/// Run the status check of one harness against its shared home.
pub async fn check_status(
    state: &Shared,
    kind: LoginKind,
    pi_provider: Option<&str>,
) -> LoginStatus {
    check_status_in(state, kind, pi_provider, &CheckHome::Shared).await
}

/// A temp dir that goes away with the value.
struct TempHome(PathBuf);

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The home for a status check of the account `sub`: a temp dir with the
/// account's credential linked in (Codex, pi), or an env var that points
/// at it (Claude). The status command reads the account and nothing else.
fn account_home(state_dir: &Path, kind: LoginKind, sub: &str) -> Result<AccountCheck, String> {
    let h = kind.harness();
    let store = crate::accounts::store_dir(state_dir, sub);
    let tmp = std::env::temp_dir().join(format!("terrarium-check-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&tmp)
        .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("cannot make the check dir: {e}"))?;
    let guard = TempHome(tmp.clone());
    let mut env = Vec::new();
    match h {
        HarnessId::Claude => env.push((
            crate::accounts::CLAUDE_CRED_ENV.to_owned(),
            store
                .join(crate::accounts::CLAUDE_DIR)
                .display()
                .to_string(),
        )),
        HarnessId::Codex | HarnessId::Pi => std::os::unix::fs::symlink(
            store.join(crate::accounts::AUTH_FILE),
            tmp.join(crate::accounts::AUTH_FILE),
        )
        .map_err(|e| format!("cannot link the account into the check dir: {e}"))?,
        HarnessId::Copilot => {}
    }
    Ok(AccountCheck {
        home: tmp,
        env,
        cred: crate::accounts::probe_home(&store, h),
        _guard: guard,
    })
}

/// The home of a status check of one account.
struct AccountCheck {
    /// The home for the status command.
    home: PathBuf,
    /// The extra env of the status command.
    env: Vec<(String, String)>,
    /// The dir that holds the credential file (for the plan).
    cred: PathBuf,
    /// Removes the temp dir.
    _guard: TempHome,
}

/// Run the status check of one harness against `at`. The status commands
/// only read: an expired login stays expired until the harness of an agent
/// refreshes it.
pub async fn check_status_in(
    state: &Shared,
    kind: LoginKind,
    pi_provider: Option<&str>,
    at: &CheckHome,
) -> LoginStatus {
    let cfg = &state.usage.config;
    let program = cfg.program(kind.harness());
    let mut extra: Vec<(String, String)> = Vec::new();
    let mut _guard = None;
    let (home, cred_home) = match at {
        CheckHome::Shared => (cfg.home(kind.harness()), cfg.home(kind.harness())),
        CheckHome::Scratch(p) => (p.clone(), p.clone()),
        CheckHome::Account(sub) => match account_home(&state.state_dir, kind, sub) {
            Ok(c) => {
                extra = c.env.clone();
                let r = (c.home.clone(), c.cred.clone());
                _guard = Some(c);
                r
            }
            Err(e) => {
                return LoginStatus {
                    kind,
                    harness: kind.harness(),
                    provider: kind.provider(pi_provider),
                    pi_provider: pi_provider.map(str::to_owned),
                    logged_in: false,
                    plan: None,
                    detail: Some(e),
                };
            }
        },
    };
    let mut st = LoginStatus {
        kind,
        harness: kind.harness(),
        provider: kind.provider(pi_provider),
        pi_provider: pi_provider.map(str::to_owned),
        logged_in: false,
        plan: None,
        detail: None,
    };
    match kind {
        LoginKind::Claude => {
            match run_cmd(&program, &["auth", "status"], kind, &home, &extra).await {
                Ok(out) => match terrarium_adapter_claude::auth::parse_auth_status(&out.stdout) {
                    Ok(a) => {
                        st.logged_in = a.logged_in;
                        let h = cred_home.clone();
                        let tier = tokio::task::spawn_blocking(move || {
                            terrarium_adapter_claude::auth::plan_tier(&h)
                        })
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten()
                        .and_then(|t| t.rate_limit_tier);
                        st.plan = claude_plan(a.subscription_type.as_deref(), tier.as_deref());
                        st.detail = a.auth_method;
                    }
                    Err(e) => st.detail = Some(e.to_string()),
                },
                Err(e) => st.detail = Some(e),
            }
        }
        LoginKind::Codex => {
            match run_cmd(&program, &["login", "status"], kind, &home, &extra).await {
                Ok(out) => {
                    let a = terrarium_adapter_codex::auth::parse_auth_status(
                        Some(out.code),
                        &out.stdout,
                        &out.stderr,
                    );
                    st.logged_in = a.logged_in;
                    if st.logged_in {
                        let p = cred_home.join("auth.json");
                        st.plan = tokio::task::spawn_blocking(move || {
                            std::fs::read_to_string(p)
                                .ok()
                                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                                .and_then(|v| codex_plan(&v))
                        })
                        .await
                        .ok()
                        .flatten();
                    }
                    // The status line, else the last line (a warning or an
                    // error). The output has no token.
                    let line = a
                        .message
                        .lines()
                        .rev()
                        .find(|l| l.trim_start().starts_with("Logged in"))
                        .or_else(|| a.message.lines().rev().find(|l| !l.trim().is_empty()))
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    st.detail = Some(line)
                        .filter(|l| !l.is_empty())
                        .or_else(|| Some(format!("no output, exit code {}", out.code)));
                }
                Err(e) => st.detail = Some(e),
            }
        }
        LoginKind::Copilot if matches!(at, CheckHome::Account(_)) => {
            // An account holds the token alone; the login name is in its
            // record.
            if let CheckHome::Account(sub) = at {
                let (dir, name) = (state.state_dir.clone(), sub.clone());
                let (token, meta) = tokio::task::spawn_blocking(move || {
                    (
                        crate::accounts::copilot_token(&dir, &name).is_some(),
                        crate::accounts::read_meta(&dir, &name),
                    )
                })
                .await
                .unwrap_or((false, None));
                st.logged_in = token;
                let who = meta
                    .and_then(|m| m.login)
                    .map(|l| format!(" for {l}"))
                    .unwrap_or_default();
                st.detail = Some(if token {
                    format!("a Copilot token{who} is in the account")
                } else {
                    "the account has no Copilot token".into()
                });
            }
        }
        LoginKind::Copilot => {
            // Copilot has no status command. A token in the plain-text
            // fallback of config.json (JSON with `//` comment lines,
            // `authTokens`) counts. Env tokens do not: the login commands
            // run without them.
            let h = home.clone();
            let found = tokio::task::spawn_blocking(move || {
                terrarium_adapter_copilot::auth::auth_status(&h, &[], &|_| None)
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
            use terrarium_adapter_copilot::auth::CredentialSource;
            match found {
                Ok(a) => {
                    st.logged_in = a.source == CredentialSource::PlainTextConfig;
                    let who = a
                        .login
                        .as_deref()
                        .map(|l| format!(" for {l}"))
                        .unwrap_or_default();
                    st.detail = Some(match a.source {
                        CredentialSource::PlainTextConfig => {
                            format!("a stored Copilot login{who} is in config.json")
                        }
                        CredentialSource::Keychain => format!(
                            "config.json names a Copilot login{who}, but its token is in the \
                             system credential store, which the daemon does not read"
                        ),
                        _ => "no stored Copilot login in config.json (the system credential \
                              store is not read)"
                            .into(),
                    });
                }
                Err(e) => st.detail = Some(e),
            }
        }
        LoginKind::Pi => {
            let provider = pi_provider.unwrap_or("anthropic");
            let args = [
                "auth",
                "check",
                "--json",
                "--no-refresh",
                "--provider",
                provider,
            ];
            match run_cmd(&program, &args, kind, &home, &extra).await {
                Ok(out) => {
                    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
                    let status = v.get("status").and_then(Value::as_str).unwrap_or_default();
                    st.logged_in = matches!(status, "ready" | "ok" | "valid")
                        || v.get("logged_in").and_then(Value::as_bool) == Some(true);
                    st.detail = v
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(|r| format!("{status}: {r}"))
                        .or(Some(status.to_owned()).filter(|s| !s.is_empty()));
                }
                Err(e) => st.detail = Some(e),
            }
        }
    }
    st
}

/// The default subscription name for a login.
pub fn default_name(st: &LoginStatus) -> String {
    let clean = |s: &str| -> String {
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_owned()
    };
    match st.kind {
        LoginKind::Copilot => "github-copilot".into(),
        _ => match st.plan.as_deref() {
            Some(p) => format!("{}-{}", st.provider, clean(p)),
            None => st.provider.clone(),
        },
    }
}

/// The definition the wizard offers for a login.
pub fn suggested_def(st: &LoginStatus, name: Option<&str>) -> SubscriptionDef {
    SubscriptionDef {
        name: name.map_or_else(|| default_name(st), str::to_owned),
        provider: st.provider.clone(),
        plan: st.plan.clone(),
        harnesses: vec![st.harness],
        // A pi login keeps its token in pi's auth.json, which the probes
        // do not read.
        probe: match st.kind {
            LoginKind::Pi => None,
            _ => crate::probes::for_provider(&st.provider).map(str::to_owned),
        },
        alert_at: 80,
        selfhosted: None,
        cloud: None,
    }
}

/// The subscription that covers a login: same provider, and the harness
/// can spend it.
pub fn covering<'a>(list: &'a [Subscription], st: &LoginStatus) -> Option<&'a Subscription> {
    list.iter()
        .find(|s| s.provider == st.provider && s.harnesses.contains(&st.harness))
}

/// One login found in the harness homes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Detected {
    #[serde(flatten)]
    pub status: LoginStatus,
    /// The subscription that covers the login, if one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    pub suggested: SubscriptionDef,
}

/// Check the home of every harness with a plan login. pi has none: it
/// uses API keys only.
pub async fn detect_all(state: &Shared) -> ApiResult<Vec<Detected>> {
    let list = state.db.read(subs::list).await?;
    let mut out = Vec::new();
    for kind in [LoginKind::Claude, LoginKind::Codex, LoginKind::Copilot] {
        let st = check_status(state, kind, None).await;
        let subscription = covering(&list, &st).map(|s| s.name.clone());
        let suggested = suggested_def(&st, None);
        out.push(Detected {
            status: st,
            subscription,
            suggested,
        });
    }
    Ok(out)
}

/// After a start: record the login state of each subscription, and offer
/// a definition for each login that has none (a `subscription.offer`
/// event).
pub async fn detect_on_start(state: &Shared) -> ApiResult<()> {
    let found = detect_all(state).await?;
    let list = state.db.read(subs::list).await?;
    for sub in &list {
        // A plan account: check the account itself (PLAN §16.3).
        if let Some(meta) = crate::accounts::read_meta(&state.state_dir, &sub.name) {
            let kind = kind_for(meta.harness);
            let pi = (kind == LoginKind::Pi).then(|| sub.provider.clone());
            let st = check_status_in(
                state,
                kind,
                pi.as_deref(),
                &CheckHome::Account(sub.name.clone()),
            )
            .await;
            let (n, status) = (sub.name.clone(), st.logged_in);
            let plan = st.plan.clone().filter(|_| st.logged_in);
            state
                .db
                .write("system", move |t| {
                    subs::record_login_check(t, &n, status)?;
                    // A renamed plan shows with no new login.
                    subs::refresh_plan(t, &n, plan.as_deref()).map(|_| ())
                })
                .await?;
            continue;
        }
        if let Some(d) = found
            .iter()
            .find(|d| d.subscription.as_deref() == Some(sub.name.as_str()))
        {
            let (n, status) = (sub.name.clone(), d.status.logged_in);
            state
                .db
                .write("system", move |t| {
                    subs::record_login_check(t, &n, status).map(|_| ())
                })
                .await?;
        }
    }
    let offers: Vec<Detected> = found
        .into_iter()
        .filter(|d| d.status.logged_in && d.subscription.is_none())
        .collect();
    if !offers.is_empty() {
        info!(
            logins = offers.len(),
            "logins without a subscription definition"
        );
        state
            .db
            .write("system", move |t| {
                for d in &offers {
                    t.emit(EventDraft::new(
                        "subscription.offer",
                        format!(
                            "{} login found with no subscription; suggested `{}`",
                            d.status.kind.as_str(),
                            d.suggested.name
                        ),
                        json!({"login": d.status, "suggested": d.suggested}),
                    ))?;
                }
                Ok(())
            })
            .await?;
    }
    Ok(())
}

/// Make or update the subscription of a finished login. A file
/// subscription keeps its definition; only its login state changes.
pub async fn register(
    state: &Shared,
    st: &LoginStatus,
    name: Option<&str>,
) -> ApiResult<Subscription> {
    let list = state.db.read(subs::list).await?;
    let existing = match name {
        Some(n) => list.iter().find(|s| s.name == n).cloned(),
        None => covering(&list, st).cloned(),
    };
    let mut def = suggested_def(st, name);
    let st2 = st.clone();
    let sub = state
        .db
        .write("operator", move |t| {
            let name = match existing {
                Some(s) if s.source == SOURCE_API => {
                    def.name = s.name.clone();
                    let mut h = s.harnesses.clone();
                    if !h.contains(&st2.harness) {
                        h.push(st2.harness);
                    }
                    def.harnesses = h;
                    def.probe = s.probe.clone().or(def.probe);
                    def.alert_at = s.alert_at;
                    def.plan = st2.plan.clone().or(s.plan.clone());
                    subs::upsert_def(t, &def, SOURCE_API)?;
                    s.name
                }
                Some(s) => {
                    if let Some(p) = &st2.plan
                        && s.plan.is_none()
                    {
                        subs::set_plan(t, &s.name, p)?;
                    }
                    s.name
                }
                None => subs::upsert_def(t, &def, SOURCE_API)?.name,
            };
            subs::set_login_status(t, &name, subs::LOGGED_IN)?;
            t.audit(
                "subscription.register",
                &name,
                None,
                json!({"kind": st2.kind, "provider": st2.provider, "plan": st2.plan}),
            )?;
            subs::get(t, &name)
        })
        .await?;
    Ok(sub)
}

/// A runner adapter for a login command: no hooks, no turns.
struct LoginAdapter {
    harness: HarnessId,
}

impl Adapter for LoginAdapter {
    fn id(&self) -> HarnessId {
        self.harness
    }
    fn detect_version(&self) -> terrarium_core::Result<semver::Version> {
        Err(terrarium_core::Error::Other(
            "a login session does not check versions".into(),
        ))
    }
    fn launch(&self, _agent: &Agent, _resume: Option<&HarnessSessionId>) -> LaunchSpec {
        LaunchSpec::new(self.harness.program(), "/")
    }
    fn hook_event(&self, _hook: &HookPayload) -> Vec<terrarium_core::Event> {
        Vec::new()
    }
}

/// One live login flow.
pub struct LoginSession {
    pub id: String,
    pub kind: LoginKind,
    pub provider: Option<String>,
    pub name: Option<String>,
    runner: Runner,
    dir: PathBuf,
    view: Mutex<LoginRow>,
    cancelled: std::sync::atomic::AtomicBool,
    /// The raw PTY output so far (the tail, at most [`MAX_LOGIN_OUTPUT`]).
    /// In memory only.
    output: Arc<Mutex<Vec<u8>>>,
}

/// The most PTY output that a login session keeps; a login screen is small.
const MAX_LOGIN_OUTPUT: usize = 512 * 1024;

/// The prefix of a login session's temp dir.
const LOGIN_DIR_PREFIX: &str = "terrarium-login-";

/// The scratch home of a login session, in its temp dir.
pub const SCRATCH_HOME: &str = "home";

/// The temp dir of login session `id`.
pub fn login_dir(id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{LOGIN_DIR_PREFIX}{id}"))
}

/// The last resort: a session that ends in any way (also a panic in its
/// task) takes its temp dir with it.
impl Drop for LoginSession {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn push_output(buf: &Mutex<Vec<u8>>, bytes: &[u8]) {
    let mut b = buf
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    b.extend_from_slice(bytes);
    if b.len() > MAX_LOGIN_OUTPUT {
        let cut = b.len() - MAX_LOGIN_OUTPUT;
        b.drain(..cut);
    }
}

impl LoginSession {
    fn view(&self) -> LoginRow {
        match self.view.lock() {
            Ok(v) => v.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    fn update(&self, f: impl FnOnce(&mut LoginRow)) -> LoginRow {
        let mut g = match self.view.lock() {
            Ok(v) => v,
            Err(p) => p.into_inner(),
        };
        f(&mut g);
        g.clone()
    }

    /// The raw PTY output so far, from memory.
    fn output(&self) -> Vec<u8> {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The live login sessions.
#[derive(Default)]
pub struct Logins {
    map: Mutex<HashMap<String, Arc<LoginSession>>>,
}

impl Logins {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<LoginSession>>> {
        match self.map.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<LoginSession>> {
        self.lock().get(id).cloned()
    }

    fn insert(&self, s: Arc<LoginSession>) {
        self.lock().insert(s.id.clone(), s);
    }

    fn remove(&self, id: &str) -> Option<Arc<LoginSession>> {
        self.lock().remove(id)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `POST /api/subscriptions/login`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartLogin {
    pub kind: LoginKind,
    /// Not used: pi, the one kind with a provider, has no login.
    #[serde(default)]
    pub provider: Option<String>,
    /// The subscription to make or update. The default comes from the
    /// status output.
    #[serde(default)]
    pub name: Option<String>,
}

/// The state of a login session, as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoginView {
    #[serde(flatten)]
    pub row: LoginRow,
    /// True when the flow ended, well or not.
    pub done: bool,
    pub instructions: String,
}

impl LoginView {
    pub fn new(row: LoginRow) -> Self {
        let kind: Option<LoginKind> = serde_json::from_value(json!(row.kind)).ok();
        let instructions = kind
            .map(|k| k.instructions(row.provider.as_deref()))
            .unwrap_or_default();
        let done = subs::FINAL_LOGIN_STATES.contains(&row.state.as_str());
        Self {
            row,
            done,
            instructions,
        }
    }
}

const MAX_LIVE_LOGINS: usize = 4;

/// Start a login session.
pub async fn start(state: &Shared, req: StartLogin) -> ApiResult<LoginView> {
    if let Some(n) = &req.name {
        subs::check_name(n)?;
    }
    // pi uses API keys only (PLAN §16.3): store a key instead.
    if req.kind == LoginKind::Pi {
        return Err(ApiError::invalid(format!(
            "{}; store a pi API key with POST /api/subscriptions/{{name}}/api-key",
            terrarium_adapter_pi::provider::PLAN_REFUSAL
        )));
    }
    if state.usage.logins.len() >= MAX_LIVE_LOGINS {
        return Err(ApiError::conflict(format!(
            "{MAX_LIVE_LOGINS} login sessions are open; finish or cancel one first"
        )));
    }
    sweep_stale_login_dirs(state);
    let id = ulid::Ulid::new().to_string();
    let cfg = &state.usage.config;
    let harness = req.kind.harness();
    let dir = login_dir(&id);
    // The login runs in a scratch home: the shared home and the other
    // accounts stay as they are (PLAN §16.3).
    let home = dir.join(SCRATCH_HOME);
    std::fs::create_dir_all(&home)
        .and_then(|()| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
        .and_then(|()| std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| ApiError::internal(format!("cannot make the login dir: {e}")))?;
    let mut spec = LaunchSpec::new(cfg.program(harness), &dir);
    spec.args = req
        .kind
        .login_args()
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    spec.env = child_env(req.kind, &home);
    spec.clear_env = true;
    let agent = AgentId::new();
    let mut rc = RunnerConfig::new(agent, &dir, &dir);
    rc.name = format!("login-{}", req.kind.as_str());
    rc.project = "terrarium".into();
    // No pty.log: the output holds the device code and the login URL. The
    // session keeps it in memory only.
    rc.log_max_bytes = 0;
    let runner = match Runner::spawn(
        rc,
        Arc::new(LoginAdapter { harness }),
        spec,
        Arc::new(NoHandler),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(ApiError::invalid(format!(
                "cannot start `{} {}`: {e}",
                cfg.program(harness),
                req.kind.login_args().join(" ")
            )));
        }
    };
    // Collect the output from the first byte: the snapshot draws what came
    // before the attach, and the stream has no gap after it.
    let output = Arc::new(Mutex::new(Vec::new()));
    let (snap, mut rx) = runner.attach();
    push_output(&output, &snap.formatted);
    let buf = output.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(chunk) => push_output(&buf, &chunk),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    let (i, k, p) = (id.clone(), req.kind, req.provider.clone());
    state
        .db
        .write("operator", move |t| {
            subs::insert_login(t, &i, k.as_str(), p.as_deref())?;
            t.audit("login.start", &i, None, json!({"kind": k, "provider": p}))
        })
        .await?;
    let row = state
        .db
        .read({
            let i = id.clone();
            move |c| subs::get_login(c, &i)
        })
        .await?;
    let session = Arc::new(LoginSession {
        id: id.clone(),
        kind: req.kind,
        provider: req.provider.clone(),
        name: req.name.clone(),
        runner,
        dir,
        view: Mutex::new(row),
        cancelled: std::sync::atomic::AtomicBool::new(false),
        output,
    });
    state.usage.logins.insert(session.clone());
    tokio::spawn(watch(state.clone(), session.clone()));
    Ok(LoginView::new(session.view()))
}

async fn persist(state: &Shared, row: LoginRow) {
    if let Err(e) = state
        .db
        .write("system", move |t| subs::update_login(t, &row))
        .await
    {
        warn!("cannot store the login session state: {e}");
    }
}

/// Remove the temp dirs of login sessions that are not live and older than
/// the login timeout: a daemon crash leaves its dirs behind. The age check
/// keeps the live sessions of another daemon on the same host.
fn sweep_stale_login_dirs(state: &Shared) {
    let max_age = state.usage.config.login_timeout + Duration::from_secs(60);
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_prefix(LOGIN_DIR_PREFIX)) else {
            continue;
        };
        // Only our own dirs: the name of a ULID. A dir of another user
        // (mode 0700 in a sticky temp dir) cannot be removed anyway.
        if id.parse::<ulid::Ulid>().is_err() || state.usage.logins.get(id).is_some() {
            continue;
        }
        let old = e.metadata().is_ok_and(|m| {
            m.is_dir()
                && m.modified()
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > max_age)
        });
        if old {
            info!(dir = %e.path().display(), "removing the temp dir of an old login session");
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Follow a login session to its end.
async fn watch(state: Shared, s: Arc<LoginSession>) {
    let timeout = state.usage.config.login_timeout;
    let started = Instant::now();
    let r = s.runner.clone();
    let mut exited = Box::pin(async move { r.wait().await });
    let mut seen = Parsed::default();
    let outcome: Result<Option<subs::Subscription>, String> = loop {
        let exit = tokio::select! {
            st = &mut exited => Some(st),
            () = tokio::time::sleep(Duration::from_millis(250)) => None,
        };
        let parsed = parse_output(s.kind, &s.output());
        if parsed != seen && (parsed.url.is_some() || parsed.device_code.is_some()) {
            seen = parsed.clone();
            let row = s.update(|v| {
                v.url = parsed.url.clone().or(v.url.take());
                v.device_code = parsed.device_code.clone().or(v.device_code.take());
                v.state = "waiting".into();
            });
            persist(&state, row).await;
        }
        if s.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            break Err("cancelled".into());
        }
        if let Some(st) = exit {
            if !st.success() {
                let screen = s.runner.screen();
                let last = screen
                    .lines
                    .iter()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .cloned()
                    .unwrap_or_default();
                break Err(format!(
                    "the login command exited with code {}: {last}",
                    st.code
                ));
            }
            break finish(&state, &s).await.map(Some);
        }
        if started.elapsed() >= timeout {
            break Err("expired".into());
        }
    };
    let _ = s.runner.kill();
    // The command is gone (or killed) before its dir goes, and the dir is
    // gone before the final state is stored: a client that sees `done`,
    // `cancelled`, or `expired` finds nothing on disk.
    let _ = tokio::time::timeout(Duration::from_secs(5), s.runner.wait()).await;
    if let Err(e) = std::fs::remove_dir_all(&s.dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(login = %s.id, "cannot remove {}: {e}", s.dir.display());
    }
    let row = s.update(|v| {
        // The code and the URL do not outlive the login (F6).
        v.url = None;
        v.device_code = None;
        outcome_into(v, &outcome, timeout);
        // With the final state, so that a reader never sees a final state
        // without its end time.
        v.finished_at = Some(crate::db::now());
    });
    let (id, st) = (row.id.clone(), row.state.clone());
    let detail =
        json!({"kind": s.kind, "state": st, "subscription": row.subscription, "error": row.error});
    persist(&state, row).await;
    let _ = state
        .db
        .write("operator", move |t| t.audit("login.end", &id, None, detail))
        .await;
    info!(login = %s.id, state = %st, "login session ended");
    state.usage.logins.remove(&s.id);
}

/// Write the final state of `outcome` into the view of a login session.
fn outcome_into(
    v: &mut LoginRow,
    outcome: &Result<Option<Subscription>, String>,
    timeout: Duration,
) {
    match outcome {
        Ok(sub) => {
            v.state = "done".into();
            v.subscription = sub.as_ref().map(|s| s.name.clone());
        }
        Err(e) if e == "cancelled" => v.state = "cancelled".into(),
        Err(e) if e == "expired" => {
            v.state = "expired".into();
            let secs = timeout.as_secs();
            v.error = Some(if secs >= 60 && secs.is_multiple_of(60) {
                format!("the login did not finish in {} minutes", secs / 60)
            } else {
                format!("the login did not finish in {secs} s")
            });
        }
        Err(e) => {
            v.state = "failed".into();
            v.error = Some(e.clone());
        }
    }
}

/// The login command finished: verify, register, probe.
async fn finish(state: &Shared, s: &LoginSession) -> Result<subs::Subscription, String> {
    let row = s.update(|v| v.state = "verifying".into());
    persist(state, row).await;
    let scratch = s.dir.join(SCRATCH_HOME);
    let st = check_status_in(
        state,
        s.kind,
        s.provider.as_deref(),
        &CheckHome::Scratch(scratch.clone()),
    )
    .await;
    if !st.logged_in {
        return Err(format!(
            "the status check does not show a login: {}",
            st.detail.unwrap_or_default()
        ));
    }
    let list = state.db.read(subs::list).await.map_err(|e| e.message)?;
    let name = login_name(&state.state_dir, &list, &st, s.name.as_deref());
    subs::check_name(&name).map_err(|e| e.message)?;
    // Only the credential leaves the scratch home, into the account of
    // `name`.
    let (dir, n, h, provider) = (
        state.state_dir.clone(),
        name.clone(),
        s.kind.harness(),
        st.provider.clone(),
    );
    tokio::task::spawn_blocking(move || {
        crate::accounts::import_login(&dir, &n, h, &provider, &scratch)
    })
    .await
    .map_err(|e| e.to_string())??;
    let sub = register(state, &st, Some(&name))
        .await
        .map_err(|e| e.message)?;
    // The first plan account of a harness becomes its default (PLAN
    // §16.3). A second account keeps the default.
    crate::api::settle_account_defaults(state, Some(sub.name.clone())).await;
    if let Err(e) = crate::probes::run_subscription(state, &sub.name).await {
        warn!(subscription = %sub.name, "first probe failed: {e}");
    }
    Ok(sub)
}

/// The state of a login session: live, or from the store.
pub async fn view(state: &Shared, id: &str) -> ApiResult<LoginView> {
    if let Some(s) = state.usage.logins.get(id) {
        return Ok(LoginView::new(s.view()));
    }
    let key = id.to_owned();
    let row = state.db.read(move |c| subs::get_login(c, &key)).await?;
    Ok(LoginView::new(row))
}

fn live(state: &Shared, id: &str) -> ApiResult<Arc<LoginSession>> {
    state.usage.logins.get(id).ok_or_else(|| {
        ApiError::new(
            crate::error::Code::NotRunning,
            format!("login session `{id}` is not live"),
        )
    })
}

/// Type a code into the flow, then Enter. The code is not stored.
pub async fn input(state: &Shared, id: &str, code: &str) -> ApiResult<()> {
    let s = live(state, id)?;
    let code = code.trim();
    if code.is_empty() || code.contains(['\r', '\n']) {
        return Err(ApiError::invalid("the code is empty or has a line break"));
    }
    let mut bytes = code.as_bytes().to_vec();
    bytes.push(b'\r');
    s.runner
        .write(&bytes)
        .await
        .map_err(|e| ApiError::conflict(format!("cannot type into the login: {e}")))?;
    let i = id.to_owned();
    let len = code.chars().count();
    state
        .db
        .write("operator", move |t| {
            t.audit("login.input", &i, None, json!({"chars": len}))
        })
        .await
}

/// Cancel a live login session.
pub async fn cancel(state: &Shared, id: &str) -> ApiResult<()> {
    let s = live(state, id)?;
    s.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = s.runner.kill();
    Ok(())
}

/// The PTY of a live login session, in the shape of an agent PTY, so the
/// agents' WebSocket code serves it.
pub fn attach(state: &Shared, id: &str) -> ApiResult<PtyAttachment> {
    let s = live(state, id)?;
    let (snap, mut out) = s.runner.attach();
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(256);
    tokio::spawn(async move {
        loop {
            match out.recv().await {
                Ok(chunk) => {
                    if out_tx.send(chunk.to_vec()).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    let (in_tx, mut in_rx) = mpsc::channel::<PtyInput>(256);
    let runner = s.runner.clone();
    tokio::spawn(async move {
        while let Some(i) = in_rx.recv().await {
            let r = match i {
                PtyInput::Bytes(b) => runner.write(&b).await,
                PtyInput::Resize { rows, cols } => runner.resize(rows, cols),
            };
            if r.is_err() {
                return;
            }
        }
    });
    Ok(PtyAttachment {
        snapshot: snap.formatted,
        output: out_rx,
        input: in_tx,
    })
}

/// Run the harness's logout command for a subscription.
pub async fn logout(state: &Shared, kind: LoginKind) -> ApiResult<String> {
    let Some(args) = kind.logout_args() else {
        return Err(ApiError::invalid(format!(
            "{} has no logout: it uses API keys only; store another key, or delete the \
             subscription",
            kind.as_str()
        )));
    };
    let cfg = &state.usage.config;
    let home = cfg.home(kind.harness());
    let out = run_cmd(&cfg.program(kind.harness()), args, kind, &home, &[])
        .await
        .map_err(ApiError::internal)?;
    let code = out.code;
    if code != 0 {
        return Err(ApiError::internal(format!(
            "`{} {}` exited with code {code}",
            kind.as_str(),
            args.join(" ")
        )));
    }
    Ok(out.first_line())
}

/// The login kind of a harness.
pub fn kind_for(h: HarnessId) -> LoginKind {
    match h {
        HarnessId::Claude => LoginKind::Claude,
        HarnessId::Codex => LoginKind::Codex,
        HarnessId::Copilot => LoginKind::Copilot,
        HarnessId::Pi => LoginKind::Pi,
    }
}

/// The name of the subscription that a new login makes or updates: the
/// `name` the operator gave, else the default name (`anthropic-max-20x`).
/// When the default subscription has a plan account already, the login is
/// a second account of the provider: `<default>-2` (or the next free
/// number).
pub fn login_name(
    state_dir: &Path,
    list: &[Subscription],
    st: &LoginStatus,
    name: Option<&str>,
) -> String {
    if let Some(n) = name {
        return n.to_owned();
    }
    let base = default_name(st);
    // A subscription can take the login when it has no account yet and
    // fits it (same provider, the harness can spend it).
    let free = |n: &str| -> bool {
        match list.iter().find(|s| s.name == n) {
            None => !crate::accounts::exists(state_dir, n),
            Some(s) => {
                !crate::accounts::exists(state_dir, n)
                    && s.provider == st.provider
                    && s.harnesses.contains(&st.harness)
            }
        }
    };
    if free(&base) {
        return base;
    }
    (2..1000)
        .map(|i| format!("{base}-{i}"))
        .find(|n| free(n))
        .unwrap_or(base)
}

/// The login kind that checks a subscription: its first harness.
pub fn kind_of(sub: &Subscription) -> Option<(LoginKind, Option<String>)> {
    let h = *sub.harnesses.first()?;
    Some(match h {
        HarnessId::Claude => (LoginKind::Claude, None),
        HarnessId::Codex => (LoginKind::Codex, None),
        HarnessId::Copilot => (LoginKind::Copilot, None),
        HarnessId::Pi => (LoginKind::Pi, Some(sub.provider.clone())),
    })
}

// ---- Stored API keys ----

/// `POST /api/subscriptions/{name}/api-key`.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKey {
    pub provider: String,
    pub key: String,
    /// `zai`: `coding` (the GLM Coding Plan, the default) or `api` (pay
    /// as you go). `moonshot`: `coding` (Kimi Code, the default),
    /// `coding-cn`, `api` (Open Platform), or `api-cn`. It becomes the
    /// subscription's plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// `bedrock`: the AWS access key ID that goes with the secret access
    /// key in `key`. Without it, `key` is an Amazon Bedrock API key. A
    /// secret like the key: never in a log or an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// `bedrock`, `gcp`, `foundry`: the `[cloud]` settings of the
    /// subscription (`terrarium_core::cloud::CloudSettings`), given with
    /// the key. The endpoint writes them to the definition; the file does
    /// not keep them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<terrarium_core::cloud::CloudSettings>,
}

impl ApiKey {
    /// The secret values of the key file, for the redaction matcher: the
    /// key, the AWS access key ID, and for a GCP service-account JSON its
    /// `private_key` and `private_key_id`.
    pub fn secret_values(&self) -> Vec<String> {
        let mut out = vec![self.key.clone()];
        out.extend(self.key_id.clone());
        if terrarium_core::cloud::kind(&self.provider) == Some(terrarium_core::cloud::Kind::Gcp)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&self.key)
        {
            for f in ["private_key", "private_key_id"] {
                if let Some(s) = v.get(f).and_then(serde_json::Value::as_str) {
                    out.push(s.to_owned());
                }
            }
        }
        out.retain(|s| !s.is_empty());
        out
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKey")
            .field("provider", &self.provider)
            .field("key", &"<redacted>")
            .field("key_id", &self.key_id.as_ref().map(|_| "<redacted>"))
            .field("plan", &self.plan)
            .field("base_url", &self.base_url)
            .field("cloud", &self.cloud)
            .finish()
    }
}

/// The env var that carries a provider's key, and the base URL var.
pub fn env_var_for(provider: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match provider {
        "anthropic" => ("ANTHROPIC_API_KEY", None),
        "openai" => ("OPENAI_API_KEY", None),
        // A self-hosted server (`terrarium_core::selfhosted`); the old word
        // `openai-compatible` is its alias. The base URL is in the
        // subscription row, not in the env.
        "selfhosted" | "openai-compatible" => (terrarium_core::selfhosted::KEY_ENV, None),
        "google" | "gemini" => ("GEMINI_API_KEY", None),
        "openrouter" => ("OPENROUTER_API_KEY", None),
        "xai" => ("XAI_API_KEY", None),
        "groq" => ("GROQ_API_KEY", None),
        "github" | "github-copilot" | "copilot" => ("COPILOT_GITHUB_TOKEN", None),
        // The base URLs are fixed per plan (`terrarium_core::zai`); no
        // base URL var.
        "zai" => (terrarium_core::zai::KEY_ENV, None),
        // pi only. The plan picks the base URL; a Kimi Code plan also gets
        // `KIMI_API_KEY` ([`credential_env_for`]).
        "moonshot" => (terrarium_core::moonshot::KEY_ENV, None),
        // Claude Code only (`terrarium_core::cloud`). Bedrock with an
        // access key ID uses `AWS_SECRET_ACCESS_KEY`
        // (`terrarium_core::cloud::Kind::key_var`). GCP: the var of the
        // JSON file.
        "bedrock" => (terrarium_core::cloud::AWS_BEARER_TOKEN_BEDROCK, None),
        "gcp" => (terrarium_core::cloud::GOOGLE_APPLICATION_CREDENTIALS, None),
        "foundry" => (terrarium_core::cloud::ANTHROPIC_FOUNDRY_API_KEY, None),
        _ => return None,
    })
}

/// `<state>/credentials/subscriptions/<name>.json`.
pub fn key_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir
        .join("credentials")
        .join("subscriptions")
        .join(format!("{name}.json"))
}

/// Write the key file, mode 0600, through a temp file and a rename.
pub fn store_key(state_dir: &Path, name: &str, key: &ApiKey) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = key_path(state_dir, name);
    let dir = path.parent().unwrap_or(state_dir);
    std::fs::create_dir_all(dir)?;
    for d in [dir, dir.parent().unwrap_or(dir)] {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = dir.join(format!(".{name}.{}.tmp", ulid::Ulid::new()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(serde_json::to_string(key).unwrap_or_default().as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}

/// The stored key of a subscription with its provider and base URL. A
/// secret: never log or return it.
pub fn read_key(state_dir: &Path, name: &str) -> Option<ApiKey> {
    let text = std::fs::read_to_string(key_path(state_dir, name)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The stored key of a subscription, for a probe.
pub fn stored_key(state_dir: &Path, name: &str) -> Option<String> {
    read_key(state_dir, name)
        .map(|k| k.key)
        .filter(|k| !k.is_empty())
}

/// The env that an agent on `sub` gets for its stored API key: the
/// provider's key var. Empty when the subscription has no stored key.
///
/// A self-hosted server (`terrarium_core::selfhosted`) gets
/// `TERRARIUM_SELFHOSTED_API_KEY`, and also `ANTHROPIC_AUTH_TOKEN` for an
/// Anthropic-style server. The base URL and the limits are no secret: the
/// adapters put them in the launch.
///
/// A Z.ai key (`terrarium_core::zai`) also gives the Claude Code env
/// ([`terrarium_core::zai::claude_env`]): `ANTHROPIC_BASE_URL`,
/// `ANTHROPIC_AUTH_TOKEN` (the key), `API_TIMEOUT_MS`, and the
/// `ANTHROPIC_DEFAULT_*_MODEL` GLM ids. Never `ANTHROPIC_API_KEY`. pi
/// reads `ZAI_API_KEY` and ignores the Anthropic vars, because its
/// `--provider` is `zai` or `zai-api`.
///
/// A Moonshot key (`terrarium_core::moonshot`, pi only) also gives
/// `KIMI_API_KEY` for a Kimi Code plan, because pi's `kimi-coding`
/// provider reads that var. Never an Anthropic var.
///
/// A cloud key (`terrarium_core::cloud`, Claude Code only) gives the env
/// of [`terrarium_core::cloud::claude_env`]: the switch
/// (`CLAUDE_CODE_USE_BEDROCK`, `_VERTEX`, `_FOUNDRY`), the region, project,
/// or resource of the `[cloud]` settings, the key vars, and the
/// `ANTHROPIC_DEFAULT_*_MODEL` vars of the model map. The GCP JSON is not
/// a var: [`credential_files_for`] gives it.
///
/// Call sites: `POST /api/agent/secrets/env`, which gives the vars to
/// `terrarium exec-env` inside the sandbox just before the harness starts,
/// and [`credential_vars_for`], which gives the launch the names only
/// (PLAN §16b.4). The values never go into the launch spec or the runner
/// spec file. They are secrets: never log them or write them to a file.
pub fn credential_env_for(state_dir: &Path, sub: &Subscription) -> Vec<(String, String)> {
    let Some(var) = sub.env_var.as_deref() else {
        return Vec::new();
    };
    let Some(k) = read_key(state_dir, &sub.name) else {
        return Vec::new();
    };
    if let Some(kind) = sub.cloud_kind() {
        let settings = sub.cloud.clone().unwrap_or_default();
        return terrarium_core::cloud::claude_env(kind, &settings, &k.key, k.key_id.as_deref())
            .vars;
    }
    let mut out = vec![(var.to_owned(), k.key.clone())];
    if sub.is_selfhosted() {
        // Claude Code reads the key of an Anthropic-style server from
        // `ANTHROPIC_AUTH_TOKEN` (over the placeholder of the launch).
        let anthropic = sub
            .selfhosted
            .as_ref()
            .is_some_and(|s| s.api == terrarium_core::selfhosted::Api::Anthropic);
        if anthropic {
            out.push(("ANTHROPIC_AUTH_TOKEN".to_owned(), k.key.clone()));
        }
        return out;
    }
    if terrarium_core::zai::is_zai(&sub.provider) || terrarium_core::zai::is_zai(&k.provider) {
        out.extend(terrarium_core::zai::claude_env(&k.key));
        return out;
    }
    if terrarium_core::moonshot::is_moonshot(&sub.provider) {
        let plan = terrarium_core::moonshot::plan_or_default(sub.plan.as_deref());
        out.extend(terrarium_core::moonshot::extra_env(plan, &k.key));
        return out;
    }
    if let (Some(url), Some((_, Some(url_var)))) = (k.base_url.as_ref(), env_var_for(&k.provider)) {
        out.push((url_var.to_owned(), url.clone()));
    }
    out
}

/// The credential files of an agent on `sub`: `(var, content)`. `terrarium
/// exec-env` writes each content to a file with mode 0600 in the sandbox
/// and sets the var to its path. Today only a GCP subscription has one: the
/// service-account JSON for `GOOGLE_APPLICATION_CREDENTIALS`. The content
/// is a secret, with the same rules as [`credential_env_for`].
pub fn credential_files_for(state_dir: &Path, sub: &Subscription) -> Vec<(String, String)> {
    let (Some(kind), Some(_)) = (sub.cloud_kind(), sub.env_var.as_deref()) else {
        return Vec::new();
    };
    let Some(k) = read_key(state_dir, &sub.name) else {
        return Vec::new();
    };
    let settings = sub.cloud.clone().unwrap_or_default();
    terrarium_core::cloud::claude_env(kind, &settings, &k.key, k.key_id.as_deref()).files
}

/// The names of [`credential_env_for`] and [`credential_files_for`], for
/// the launch. The values are zeroed here.
pub fn credential_vars_for(state_dir: &Path, sub: &Subscription) -> Vec<String> {
    credential_env_for(state_dir, sub)
        .into_iter()
        .chain(credential_files_for(state_dir, sub))
        .map(|(k, v)| {
            drop(zeroize::Zeroizing::new(v));
            k
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A login command never gets the daemon's token or the first password.
    #[test]
    fn child_env_strips_the_daemon_secrets() {
        for k in [
            "TERRARIUM_TOKEN",
            crate::password::ENV_PASSWORD,
            crate::password::ENV_PASSWORD_FILE,
        ] {
            assert!(STRIP_VARS.contains(&k), "{k}");
        }
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/login/{name}.raw",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn claude_url_from_the_capture() {
        let p = parse_output(LoginKind::Claude, &fixture("claude"));
        let url = p.url.unwrap();
        assert!(
            url.starts_with("https://claude.com/cai/oauth/authorize?code=true&client_id="),
            "{url}"
        );
        assert!(
            url.ends_with("state=STATEstateSTATEstateSTATEstate0123456789"),
            "{url}"
        );
        assert_eq!(p.device_code, None);
        // Without the OSC 8 link, the visible text still gives the URL.
        let plain = strip_ansi(&String::from_utf8_lossy(&fixture("claude")));
        let p = parse_output(LoginKind::Claude, plain.as_bytes());
        assert!(p.url.unwrap().contains("/oauth/authorize?"));
    }

    #[test]
    fn codex_device_code_from_the_capture() {
        let p = parse_output(LoginKind::Codex, &fixture("codex"));
        assert_eq!(
            p.url.as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert_eq!(p.device_code.as_deref(), Some("ABCD-EF12G"));
    }

    #[test]
    fn copilot_device_code_from_the_capture() {
        let p = parse_output(LoginKind::Copilot, &fixture("copilot"));
        assert_eq!(p.url.as_deref(), Some("https://github.com/login/device"));
        assert_eq!(p.device_code.as_deref(), Some("WXYZ-1234"));
    }

    #[test]
    fn nothing_before_the_prompt() {
        for k in [
            LoginKind::Claude,
            LoginKind::Codex,
            LoginKind::Copilot,
            LoginKind::Pi,
        ] {
            assert_eq!(
                parse_output(k, b"Opening browser to sign in...\r\n"),
                Parsed::default()
            );
        }
    }

    #[test]
    fn plans_and_names() {
        // The Max 20x plan is the Ultra plan since 2026-10-30: the login
        // of 2026-11-01 still has the old tier.
        assert_eq!(
            claude_plan(Some("max"), Some("default_claude_max_20x")).as_deref(),
            Some("Ultra")
        );
        assert_eq!(
            claude_plan(Some("max"), Some("default_claude_ultra")).as_deref(),
            Some("Ultra")
        );
        assert_eq!(
            claude_plan(Some("max"), Some("default_claude_max_5x")).as_deref(),
            Some("Max")
        );
        assert_eq!(claude_plan(Some("max"), None).as_deref(), Some("Max"));
        assert_eq!(claude_plan(Some("pro"), None).as_deref(), Some("Pro"));
        assert_eq!(claude_plan(Some("odd"), None).as_deref(), Some("Odd"));
        assert_eq!(claude_plan(None, None), None);
        let st = LoginStatus {
            kind: LoginKind::Claude,
            harness: HarnessId::Claude,
            provider: "anthropic".into(),
            pi_provider: None,
            logged_in: true,
            plan: Some("Max 20x".into()),
            detail: None,
        };
        assert_eq!(default_name(&st), "anthropic-max-20x");
        let def = suggested_def(&st, None);
        assert_eq!(def.probe.as_deref(), Some("anthropic-oauth-usage"));
        assert_eq!(def.harnesses, [HarnessId::Claude]);
    }

    #[test]
    fn codex_plan_reads_only_the_claim() {
        use base64::Engine;
        let claims = json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}});
        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        let auth = json!({"tokens": {"id_token": format!("h.{p}.s")}});
        assert_eq!(codex_plan(&auth).as_deref(), Some("pro"));
        assert_eq!(codex_plan(&json!({"tokens": {"id_token": "bad"}})), None);
    }

    #[test]
    fn keys_are_0600_and_feed_the_env() {
        let tmp = tempfile::tempdir().unwrap();
        let key = ApiKey {
            provider: "openai-compatible".into(),
            key: "sk-test".into(),
            plan: None,
            base_url: Some("https://llm.example/v1".into()),
            models: vec!["m".into()],
            key_id: None,
            cloud: None,
        };
        assert!(!format!("{key:?}").contains("sk-test"));
        store_key(tmp.path(), "local", &key).unwrap();
        let path = key_path(tmp.path(), "local");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(stored_key(tmp.path(), "local").as_deref(), Some("sk-test"));
        let mut sub: Subscription = serde_json::from_value(json!({
            "name": "local", "provider": "openai-compatible", "harnesses": ["pi"], "alert_at": 80,
            "source": "api", "login_status": "unknown", "probe_status": "none",
            "created_at": "", "updated_at": ""
        }))
        .unwrap();
        assert!(credential_env_for(tmp.path(), &sub).is_empty());
        // The old word is an alias of `selfhosted`: only the key var, no
        // base URL var (the base URL is in the row).
        assert_eq!(
            env_var_for("openai-compatible"),
            Some(("TERRARIUM_SELFHOSTED_API_KEY", None))
        );
        sub.provider = "selfhosted".into();
        sub.env_var = Some("TERRARIUM_SELFHOSTED_API_KEY".into());
        sub.selfhosted = Some(terrarium_core::selfhosted::SelfHosted {
            base_url: "https://llm.example/v1".into(),
            ..Default::default()
        });
        assert_eq!(
            credential_env_for(tmp.path(), &sub),
            [(
                "TERRARIUM_SELFHOSTED_API_KEY".to_owned(),
                "sk-test".to_owned()
            )]
        );
        // An Anthropic-style server: Claude Code reads ANTHROPIC_AUTH_TOKEN.
        if let Some(s) = sub.selfhosted.as_mut() {
            s.api = terrarium_core::selfhosted::Api::Anthropic;
        }
        assert_eq!(
            credential_env_for(tmp.path(), &sub),
            [
                (
                    "TERRARIUM_SELFHOSTED_API_KEY".to_owned(),
                    "sk-test".to_owned()
                ),
                ("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-test".to_owned())
            ]
        );
        assert_eq!(env_var_for("anthropic"), Some(("ANTHROPIC_API_KEY", None)));
        assert_eq!(env_var_for("nope"), None);
    }

    #[test]
    fn zai_key_feeds_claude_and_pi_env() {
        let tmp = tempfile::tempdir().unwrap();
        let key = ApiKey {
            provider: "zai".into(),
            key: "zk-test".into(),
            plan: Some("coding".into()),
            base_url: None,
            models: vec![],
            key_id: None,
            cloud: None,
        };
        store_key(tmp.path(), "glm", &key).unwrap();
        let mut sub: Subscription = serde_json::from_value(json!({
            "name": "glm", "provider": "zai", "plan": "coding", "harnesses": ["claude", "pi"],
            "alert_at": 80, "source": "api", "login_status": "api_key", "probe_status": "none",
            "created_at": "", "updated_at": ""
        }))
        .unwrap();
        assert!(credential_env_for(tmp.path(), &sub).is_empty());
        assert_eq!(env_var_for("zai"), Some(("ZAI_API_KEY", None)));
        sub.env_var = Some("ZAI_API_KEY".into());
        let env: std::collections::BTreeMap<String, String> =
            credential_env_for(tmp.path(), &sub).into_iter().collect();
        assert_eq!(env["ZAI_API_KEY"], "zk-test");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "zk-test");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://api.z.ai/api/anthropic");
        assert_eq!(env["API_TIMEOUT_MS"], "3000000");
        assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "glm-5.3-flash");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-5.3");
        assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "glm-5.3");
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("OPENAI_BASE_URL"));
        // The key never shows in the debug form.
        assert!(!format!("{key:?}").contains("zk-test"));
    }

    #[test]
    fn moonshot_key_feeds_pi_env_only() {
        let tmp = tempfile::tempdir().unwrap();
        let key = ApiKey {
            provider: "moonshot".into(),
            key: "sk-ms-test".into(),
            plan: Some("coding".into()),
            base_url: None,
            models: vec![],
            key_id: None,
            cloud: None,
        };
        store_key(tmp.path(), "kimi", &key).unwrap();
        let mut sub: Subscription = serde_json::from_value(json!({
            "name": "kimi", "provider": "moonshot", "plan": "coding", "harnesses": ["pi"],
            "alert_at": 80, "source": "api", "login_status": "api_key", "probe_status": "none",
            "created_at": "", "updated_at": ""
        }))
        .unwrap();
        assert!(credential_env_for(tmp.path(), &sub).is_empty());
        assert_eq!(env_var_for("moonshot"), Some(("MOONSHOT_API_KEY", None)));
        sub.env_var = Some("MOONSHOT_API_KEY".into());
        let env = |sub: &Subscription| -> std::collections::BTreeMap<String, String> {
            credential_env_for(tmp.path(), sub).into_iter().collect()
        };
        // A Kimi Code plan: pi's `kimi-coding` reads KIMI_API_KEY.
        let e = env(&sub);
        assert_eq!(e["MOONSHOT_API_KEY"], "sk-ms-test");
        assert_eq!(e["KIMI_API_KEY"], "sk-ms-test");
        assert_eq!(e.len(), 2, "{:?}", e.keys());
        // An Open Platform plan: MOONSHOT_API_KEY only.
        sub.plan = Some("api-cn".into());
        let e = env(&sub);
        assert_eq!(e.keys().collect::<Vec<_>>(), ["MOONSHOT_API_KEY"]);
        // Never a Claude Code var.
        assert!(!e.keys().any(|k| k.starts_with("ANTHROPIC_")));
    }
}
