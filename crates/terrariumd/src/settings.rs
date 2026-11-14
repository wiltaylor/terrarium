use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use terrarium_core::{HarnessId, paths};

use crate::runner_supervisor::RunnerMode;
use crate::sandbox::SandboxMode;

/// The Terrarium daemon. Every option also reads an env var.
#[derive(Debug, Parser)]
#[command(
    name = "terrariumd",
    version = terrarium_version::FULL,
    long_version = terrarium_version::LONG,
    about
)]
pub struct Settings {
    /// Updater drain wait, also the cap for graceful agent RPC draining.
    #[arg(long, env = "TERRARIUM_UPDATE_DRAIN_WAIT", default_value_t = 600)]
    pub update_drain_wait: u64,
    /// The Unix socket for the operator CLI. No token is needed on it.
    #[arg(long, env = paths::ENV_SOCKET, default_value_os_t = paths::api_socket())]
    pub socket: PathBuf,

    /// The TCP address for the dashboard and the remote API.
    #[arg(long, env = paths::ENV_LISTEN, default_value = paths::DEFAULT_LISTEN)]
    pub listen: String,

    /// The bearer token for the TCP listener. Without it the TCP listener stays off.
    #[arg(long, env = paths::ENV_TOKEN, hide_env_values = true)]
    pub token: Option<String>,

    /// SQLite, workspaces, logs, and artifacts.
    #[arg(long, env = paths::ENV_STATE_DIR, default_value = paths::DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,

    /// Definitions: projects/, agents/, targets/, subscriptions/.
    #[arg(long, env = paths::ENV_CONFIG_DIR, default_value = paths::DEFAULT_CONFIG_DIR)]
    pub config_dir: PathBuf,

    /// The TCP address of the artifact serving origin (PLAN §15.2). Empty:
    /// no listener of its own, and the dashboard port serves `/a/...`
    /// instead. With a listener (the default), the dashboard port does not
    /// serve artifacts (`docs/security-review.md`, F5).
    #[arg(long, env = ENV_ARTIFACT_LISTEN, default_value = DEFAULT_ARTIFACT_LISTEN)]
    pub artifact_listen: String,

    /// The TCP address of the git proxy (issue #11, `crate::git_proxy`):
    /// the agents' checkouts reach the forge through it, with no token in
    /// the sandbox. Loopback by default: the sandboxes share the network
    /// of the daemon. Empty: no proxy, and every repo is `direct`.
    #[arg(long, env = ENV_GIT_PROXY_LISTEN, default_value = DEFAULT_GIT_PROXY_LISTEN)]
    pub git_proxy_listen: String,

    /// The dashboard URL as the operator opens it, for example
    /// `https://terrarium.example.com`. It goes into mail links and
    /// `Version.url`, the WebSocket `Origin` check of a cookie session, and
    /// the CORS answer of the artifact origin.
    #[arg(long, env = ENV_PUBLIC_URL)]
    pub public_url: Option<String>,

    /// The artifact origin as a browser opens it, for example
    /// `https://artifacts.example.com`. Signed artifact URLs and the
    /// dashboard CSP name it. Default: the request host with the port of
    /// `--artifact-listen`.
    #[arg(long, env = ENV_ARTIFACT_PUBLIC_URL)]
    pub artifact_public_url: Option<String>,

    /// The directory of the static host agent binaries that
    /// `GET /install/terrarium-host` serves: `terrarium-host-<arch>-linux`
    /// and its `.sha256` (the image puts them in `/opt/terrarium/dist`).
    #[arg(long, env = ENV_HOST_DIST_DIR, default_value = DEFAULT_HOST_DIST_DIR)]
    pub host_dist_dir: PathBuf,

    /// The host listener as a host agent dials it, for example
    /// `wss://terrarium.lan:8082`. The installer at `GET /install/host.sh`
    /// writes it as `daemon_url`. Default: `wss://` with the host of the
    /// public URL and the port of the host listener, when the listener
    /// speaks TLS.
    #[arg(long, env = ENV_HOST_DAEMON_URL)]
    pub host_daemon_url: Option<String>,

    /// Take the bearer token as `?token=` on a WebSocket upgrade
    /// (deprecated: the query string reaches logs). `0` (the default)
    /// refuses it; clients send `Authorization` or the subprotocol
    /// `bearer.<token>`.
    #[arg(
        long,
        env = ENV_WS_QUERY_TOKEN,
        default_value = "0",
        action = clap::ArgAction::Set,
        value_parser = parse_switch
    )]
    pub ws_query_token: bool,

    /// Staged project pages capability. Activation belongs to TER-1077.
    #[arg(long, env = "TERRARIUM_PROJECT_PAGES_V2", default_value = "0",
        action = clap::ArgAction::Set, value_parser = parse_switch)]
    pub project_pages_v2: bool,

    /// The column rules (TER-1117, kanban rework C5): at each load of the
    /// config the daemon converts every board that does not run on column
    /// rules yet, and a new project gets the standard board. On by
    /// default; `0` keeps the legacy flows (no per-project fallback). C2b
    /// (TER-1172) removes the switch with the legacy flows.
    #[arg(long, env = "TERRARIUM_BOARD_COLUMNS", default_value = "1",
        action = clap::ArgAction::Set, value_parser = parse_switch)]
    pub board_columns: bool,

    /// The life of a dashboard session cookie (`POST /api/session`):
    /// seconds, or a number with `s`, `m`, `h`, or `d`. A cookie past half
    /// its life is renewed on the next request.
    #[arg(
        long,
        env = ENV_SESSION_TTL,
        default_value = DEFAULT_SESSION_TTL,
        value_parser = parse_duration
    )]
    pub session_ttl: std::time::Duration,

    /// The life of a CLI session token (`terrarium login`), as
    /// `--session-ttl`. It is not renewed.
    #[arg(
        long,
        env = ENV_CLI_SESSION_TTL,
        default_value = DEFAULT_CLI_SESSION_TTL,
        value_parser = parse_duration
    )]
    pub cli_session_ttl: std::time::Duration,

    /// The longest life of a dashboard session through its renewals,
    /// counted from the login, as `--session-ttl`.
    #[arg(
        long,
        env = ENV_SESSION_MAX_AGE,
        default_value = DEFAULT_SESSION_MAX_AGE,
        value_parser = parse_duration
    )]
    pub session_max_age: std::time::Duration,

    /// A file that holds the first operator password (its first line), as
    /// `TERRARIUM_PASSWORD`.
    #[arg(long, env = crate::password::ENV_PASSWORD_FILE)]
    pub password_file: Option<PathBuf>,

    /// Runner sockets go in `<dir>/agents/<ulid>.sock`.
    #[arg(long, env = "TERRARIUM_RUNTIME_DIR", default_value_os_t = default_runtime_dir())]
    pub runtime_dir: PathBuf,

    /// The prompt templates and personas, `<dir>/<name>.md`. Default: the
    /// `prompts/` of the config repo.
    #[arg(long, env = "TERRARIUM_PROMPTS_DIR")]
    pub prompts_dir: Option<PathBuf>,

    /// The config repo (PLAN §8.3): `prompts/`, `harness/<harness>/`, and
    /// `export/`, in git. The daemon copies the packaged defaults into it
    /// at each start, and makes it a git repo.
    #[arg(long, env = "TERRARIUM_CONFIG_REPO", default_value = crate::config_repo::DEFAULT_DIR)]
    pub config_repo: PathBuf,

    /// The packaged defaults: `manifest.json` and the files it lists (the
    /// image has them in `/opt/terrarium/defaults`). A dir with no
    /// manifest: no seeding.
    #[arg(long, env = "TERRARIUM_DEFAULTS_DIR", default_value = crate::defaults::DEFAULT_DEFAULTS_DIR)]
    pub defaults_dir: PathBuf,

    /// The time between two automatic runs of `TERRARIUM_CONFIG_SYNC`
    /// (`pull` or `sync`; default `off`): seconds, or a number with `s`,
    /// `m`, `h`, or `d`.
    #[arg(
        long,
        env = crate::config_repo::ENV_SYNC_INTERVAL,
        default_value = "15m",
        value_parser = parse_duration
    )]
    pub config_sync_interval: std::time::Duration,

    /// How harnesses run: `bwrap` (the sandbox of PLAN §11) or `none`
    /// (development and tests only).
    #[arg(long, env = "TERRARIUM_SANDBOX", default_value = "bwrap")]
    pub sandbox: SandboxMode,

    /// The local repos. A project repo whose URL is a path under this dir
    /// (`file:///repos/<name>.git`) is bound read-write into the sandbox of
    /// each agent of that project, at the same path.
    #[arg(long, env = "TERRARIUM_REPOS_DIR", default_value = crate::sandbox::DEFAULT_REPOS_DIR)]
    pub repos_dir: PathBuf,

    /// The work root. The workspace of an agent is
    /// `<dir>/<project>/<agent>`; the sandbox binds it at `$HOME/work` and
    /// at `/work/<project>/<agent>`, and a host agent attaches it to a
    /// target at `/work/<project>/<agent>`. The host agent's `work_dir` is
    /// the host path of this dir.
    #[arg(long, env = "TERRARIUM_WORK_DIR", default_value = crate::workspace::DEFAULT_WORK_DIR)]
    pub work_dir: PathBuf,

    /// The most targets one project may have. A `POST /api/targets` past
    /// it is `409 conflict`.
    #[arg(long, env = "TERRARIUM_MAX_TARGETS_PER_PROJECT", default_value_t = DEFAULT_MAX_TARGETS_PER_PROJECT)]
    pub max_targets_per_project: usize,

    /// Pass `--new-session` to bwrap. Off by default, because the harness
    /// then misses terminal resizes (docs/compat.md).
    #[arg(long, env = "TERRARIUM_SANDBOX_NEW_SESSION")]
    pub sandbox_new_session: bool,

    /// The shared home of a harness on this machine, as `<harness>=<dir>`.
    /// The default is `/auth/<harness>`. Repeat it, or use commas.
    #[arg(
        long = "harness-home",
        env = "TERRARIUM_HARNESS_HOME",
        value_delimiter = ',',
        value_parser = parse_home
    )]
    pub harness_homes: Vec<(HarnessId, PathBuf)>,

    /// The `terrarium` CLI that the sandbox binds for hooks and agent
    /// commands. The default is `terrarium` next to this binary, else on PATH.
    #[arg(long, env = "TERRARIUM_CLI")]
    pub cli: Option<PathBuf>,

    /// How agent runners run: `process` (one `terrarium-runner` process per
    /// agent; agents live through a daemon restart) or `inprocess` (in the
    /// daemon; the agents end with it; tests and development only).
    #[arg(long, env = "TERRARIUM_RUNNER_MODE", value_enum, default_value_t = RunnerModeName::Process)]
    pub runner_mode: RunnerModeName,

    /// The `terrarium-runner` binary for `--runner-mode process`. The
    /// default is `terrarium-runner` next to this binary, else on PATH.
    #[arg(long, env = "TERRARIUM_RUNNER")]
    pub runner: Option<PathBuf>,

    /// The background check for harness updates: `daily`, `off`, or a
    /// duration of 1h or more (`6h`). Each harness with the policy `auto`
    /// moves to the latest release on npm (`crate::harness_auto_update`).
    #[arg(
        long,
        env = crate::harness_auto_update::ENV_AUTO_UPDATE,
        default_value = "daily"
    )]
    pub harness_auto_update: crate::harness_auto_update::Schedule,

    /// The operator mail for each update that the auto-update installs:
    /// `off` (the default: no mail) or `on`. A failed install mails in
    /// both cases.
    #[arg(
        long,
        env = crate::harness_auto_update::ENV_UPDATE_MAIL,
        value_enum,
        default_value_t = crate::harness_auto_update::UpdateMail::Off
    )]
    pub harness_update_mail: crate::harness_auto_update::UpdateMail,

    /// The time that an Inbox thread waits on the operator with no
    /// activity before its one Inbox reminder (TER-95): a duration (`3d`,
    /// `12h`), or `off` (also `0`).
    #[arg(
        long,
        env = crate::thread_close::ENV_REMINDER,
        default_value = "3d",
        value_parser = crate::thread_close::parse_setting
    )]
    pub thread_reminder: std::time::Duration,

    /// The time that an Inbox thread waits on the operator with no
    /// activity before the daemon closes it as stale and mails its
    /// report-to lead (TER-95): a duration (`14d`), or `off` (also `0`).
    #[arg(
        long,
        env = crate::thread_close::ENV_STALE,
        default_value = "14d",
        value_parser = crate::thread_close::parse_setting
    )]
    pub thread_stale: std::time::Duration,

    /// The quota probe samples younger than this stay (TER-395): a
    /// duration of at least `24h`, the default history of
    /// `GET /api/subscriptions/{name}/quota`. The newest sample of each
    /// window stays at any age.
    #[arg(
        long,
        env = "TERRARIUM_QUOTA_SAMPLE_KEEP",
        default_value = "48h",
        value_parser = parse_sample_keep
    )]
    pub quota_sample_keep: std::time::Duration,

    /// The tools dir: the npm prefix that holds the harnesses
    /// (`<dir>/bin` first on `PATH`). At start the daemon installs each pin
    /// of `<config dir>/harnesses.toml` that is missing there, and
    /// `POST /api/harnesses/{name}/update` updates in place. A relative path
    /// is from the cwd. Empty: no tools dir; the harnesses come from `PATH`.
    #[arg(long, env = crate::harness_tools::ENV_TOOLS_DIR, default_value = crate::harness_tools::DEFAULT_TOOLS_DIR)]
    pub tools_dir: String,

    /// Do not install the missing pins into the tools dir at start.
    #[arg(long, env = "TERRARIUM_NO_HARNESS_INSTALL")]
    pub no_harness_install: bool,

    /// The npm program for the tools dir.
    #[arg(long, env = "TERRARIUM_NPM", default_value = "npm")]
    pub npm: PathBuf,

    /// Trust the `X-Terrarium-Agent` header on the Unix socket. Any local
    /// caller can then act as any agent. For tests and development only.
    #[arg(long, env = "TERRARIUM_INSECURE_AGENT_HEADER")]
    pub insecure_agent_header: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The default of `TERRARIUM_MAX_TARGETS_PER_PROJECT`.
pub const DEFAULT_MAX_TARGETS_PER_PROJECT: usize = 8;

/// The values of `--runner-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RunnerModeName {
    Process,
    Inprocess,
}

/// Debug commands. Without one, the daemon serves.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Read-only check of the templates this daemon requires before an update.
    RequiredTemplatesCheck,
    /// Print the environment key names of the daemon service in this
    /// build's compose.yaml, and the keys it marks `# update:required`, as
    /// one JSON line. No values. The updater runs it in the new image
    /// before an update (TER-1209).
    ReleaseEnvKeys,
    /// Print the full sandbox command for an agent, and exit. It writes the
    /// agent's prompt and harness files, as a start does, but starts nothing.
    SandboxDryRun {
        /// The agent: a ULID, `project/name`, or a unique name.
        agent: String,
    },
    /// The harnesses in the tools dir (`--tools-dir`), with no daemon.
    Harness {
        #[command(subcommand)]
        command: HarnessCommand,
    },
    /// The operator password in the state dir (`--state-dir`), with no
    /// daemon. A running daemon sees the change at once.
    Password {
        #[command(subcommand)]
        command: PasswordCommand,
    },
    /// The secret store in the state dir (PLAN §16b), with no daemon:
    /// stop the daemon first.
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// The packaged defaults and the config repo (`--defaults-dir`,
    /// `--config-repo`), with no daemon.
    Defaults {
        #[command(subcommand)]
        command: DefaultsCommand,
    },
    /// The config repo, with no daemon.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Backups of the daemon's data in zip files (docs/api.md §32). The
    /// files hold secrets.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
}

/// The passphrase of a sealed backup.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct PassphraseArgs {
    /// Read the passphrase from the first line of this file.
    #[arg(long, conflicts_with = "passphrase_stdin")]
    pub passphrase_file: Option<PathBuf>,
    /// Read the passphrase from the first line of stdin.
    #[arg(long)]
    pub passphrase_stdin: bool,
}

/// `terrariumd backup ...`.
#[derive(Debug, Subcommand)]
pub enum BackupCommand {
    /// Make a backup: the state dir (with the database copy and the
    /// secrets), the config repo, and with `--work` the workspaces. A
    /// running daemon can stay up.
    Create {
        /// `full`, or `diff`: the files that changed since the newest full
        /// backup (or `--base`).
        #[arg(long, value_enum, default_value_t = crate::backup::Kind::Full)]
        kind: crate::backup::Kind,
        /// Add the agent workspaces (`--work-dir`).
        #[arg(long)]
        work: bool,
        /// The base of a diff. Default: the newest full backup in the dir.
        #[arg(long)]
        base: Option<PathBuf>,
        /// The backup dir. Default: `TERRARIUM_BACKUP_DIR`, else
        /// `<state>/backups`.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Seal the backup with a passphrase: ask for it on the terminal,
        /// twice. Or give `--passphrase-file` or `--passphrase-stdin`.
        #[arg(long)]
        encrypt: bool,
        #[command(flatten)]
        passphrase: PassphraseArgs,
        #[arg(long)]
        json: bool,
    },
    /// List the backups in the backup dir, oldest first.
    Ls {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Read every file of a backup (and of the base of a diff) and check
    /// it against the manifest.
    Verify {
        file: PathBuf,
        /// The base of a diff. Default: the backup with the base ID in the
        /// dir of the diff.
        #[arg(long)]
        base: Option<PathBuf>,
        #[command(flatten)]
        passphrase: PassphraseArgs,
        #[arg(long)]
        json: bool,
    },
    /// The restore drill: restore a backup into an empty scratch dir and
    /// check it (every file, the database integrity and schema version,
    /// `secrets.key`). The live data does not change; a daemon can run.
    /// The scratch dir holds the secrets in plain form: it is deleted at
    /// the end unless `--keep`.
    Drill {
        file: PathBuf,
        /// The base of a diff. Default: the backup with the base ID in the
        /// dir of the diff.
        #[arg(long)]
        base: Option<PathBuf>,
        /// The scratch dir (empty or missing). Default: a new
        /// `.drill-<id>` dir in the backup dir.
        #[arg(long)]
        into: Option<PathBuf>,
        /// Leave the scratch dir.
        #[arg(long)]
        keep: bool,
        #[command(flatten)]
        passphrase: PassphraseArgs,
        #[arg(long)]
        json: bool,
    },
    /// Restore a backup (a full, or a diff and its base) into the state
    /// dir, the config repo, and the work dir. Stop the daemon first. The
    /// old content of each dir moves to `.terrarium-pre-restore-<time>/`.
    Restore {
        file: PathBuf,
        #[arg(long)]
        base: Option<PathBuf>,
        /// Leave the work dir as it is.
        #[arg(long)]
        skip_work: bool,
        #[command(flatten)]
        passphrase: PassphraseArgs,
        #[arg(long)]
        json: bool,
    },
}

/// `terrariumd defaults ...`.
#[derive(Debug, Subcommand)]
pub enum DefaultsCommand {
    /// Copy the missing defaults into the config repo, upgrade the
    /// unedited ones, and write `<file>.new` next to the edited ones (what
    /// each start does).
    Seed,
    /// Back the config repo up to `<state>/backup/`, then write every
    /// default over it (`just defaults::apply`). The operator's own files
    /// and `.git` stay.
    Apply,
    /// Write `manifest.json` of a defaults dir (`just defaults::manifest`).
    Manifest {
        /// The defaults dir. Default: `--defaults-dir`.
        dir: Option<PathBuf>,
    },
}

/// `terrariumd config ...`.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// The secret scan. `--staged`: what the next commit takes (the
    /// `pre-commit` hook). Else the named files. Exit 1 on a finding; the
    /// output names the file, the line, and the rule, never the value.
    Scan {
        #[arg(long)]
        staged: bool,
        /// The repo for `--staged`. Default: the current dir.
        #[arg(long)]
        repo: Option<PathBuf>,
        files: Vec<PathBuf>,
    },
}

/// `terrariumd password ...`.
#[derive(Debug, Subcommand)]
pub enum PasswordCommand {
    /// Set the operator password (12 characters or more). It asks on the
    /// terminal, twice, with no echo. Every session ends.
    Set {
        /// Read the password from the first line of stdin, for scripts.
        #[arg(long)]
        stdin: bool,
    },
}

/// `terrariumd secrets ...`.
#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Make a new master key (`<state>/secrets.key`) and seal each secret
    /// again with it, in one transaction. The old key stops working. Stop
    /// the daemon first.
    RotateKey,
}

/// `terrariumd harness ...`.
#[derive(Debug, Subcommand)]
pub enum HarnessCommand {
    /// Install each pin of harnesses.toml that is missing from the tools
    /// dir, and the Copilot prompt extension when it changed. A present
    /// package stays at its version. Exit 1 when an install fails.
    Install {
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show each pin, the installed version, and the manifest row.
    Status {
        /// Print the status as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn default_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|d| PathBuf::from(d).join("terrarium"))
        .unwrap_or_else(|| PathBuf::from("/run/terrarium"))
}

fn parse_home(s: &str) -> Result<(HarnessId, PathBuf), String> {
    let (h, dir) = s
        .split_once('=')
        .ok_or_else(|| format!("`{s}` is not `<harness>=<dir>`"))?;
    Ok((h.parse()?, PathBuf::from(dir)))
}

/// `program` next to the running binary, else on PATH.
fn sibling_or_path(program: &str) -> Option<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(program)))
        .filter(|p| p.is_file());
    sibling.or_else(|| crate::sandbox::which(program, std::env::var("PATH").ok().as_deref()))
}

impl Settings {
    /// The prompts dir: `--prompts-dir`, else `<config repo>/prompts`.
    pub fn prompts_dir(&self) -> PathBuf {
        self.prompts_dir
            .clone()
            .unwrap_or_else(|| self.config_repo.join("prompts"))
    }

    /// The tools dir and its pins, or `None` when `--tools-dir` is empty.
    /// The pins come from `<config dir>/harnesses.toml`, else from the
    /// pins built into this binary.
    pub fn tools_config(&self) -> Option<crate::harness_tools::ToolsConfig> {
        let dir = self.tools_dir.trim();
        if dir.is_empty() {
            return None;
        }
        let dir = std::path::absolute(dir).unwrap_or_else(|_| PathBuf::from(dir));
        let (pins, source) =
            crate::harness_tools::load_pins(&self.config_dir.join(crate::harness_tools::PINS_FILE));
        tracing::debug!(pins = %source, "harness pins");
        let copilot_home = self
            .harness_homes
            .iter()
            .find(|(h, _)| *h == HarnessId::Copilot)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| PathBuf::from(HarnessId::Copilot.auth_dir()));
        let mut cfg = crate::harness_tools::ToolsConfig::new(dir, pins, copilot_home);
        cfg.npm = self.npm.clone();
        Some(cfg)
    }

    /// The runner mode. This is the one place that picks it
    /// (`TERRARIUM_RUNNER_MODE`, `TERRARIUM_RUNNER`).
    pub fn runner_mode(&self) -> Result<RunnerMode, String> {
        match self.runner_mode {
            RunnerModeName::Inprocess => Ok(RunnerMode::InProcess),
            RunnerModeName::Process => {
                let bin = match &self.runner {
                    Some(p) => p.clone(),
                    None => sibling_or_path("terrarium-runner").ok_or_else(|| {
                        "no `terrarium-runner` binary next to terrariumd or on PATH; build it \
                         (`cargo build -p terrarium-runner`), set TERRARIUM_RUNNER, or set \
                         TERRARIUM_RUNNER_MODE=inprocess for development"
                            .to_owned()
                    })?,
                };
                if !bin.is_file() {
                    return Err(format!(
                        "the runner binary {} does not exist; set TERRARIUM_RUNNER",
                        bin.display()
                    ));
                }
                Ok(RunnerMode::Process { bin })
            }
        }
    }

    /// The `terrarium` CLI to bind: `--cli`, else next to this binary, else
    /// on PATH.
    pub fn cli_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.cli {
            return Some(p.clone());
        }
        sibling_or_path("terrarium")
    }
}

// ---- the web settings: origins, WebSocket auth, sessions ----
//
// `Settings` parses them (flag or env) in one place; `Settings::web` turns
// them into a `WebConfig`; `DaemonConfig` carries it to the routers and the
// mail code. No other code reads these env vars.

/// `TERRARIUM_PUBLIC_URL`: the dashboard as the operator opens it, for the
/// links in operator mail, `Version.url`, the WebSocket `Origin` check, and
/// the CORS answer of the artifact origin.
pub const ENV_PUBLIC_URL: &str = "TERRARIUM_PUBLIC_URL";
/// The mail-link base when [`ENV_PUBLIC_URL`] is not set.
pub const DEFAULT_PUBLIC_URL: &str = "http://localhost:8080";
/// `TERRARIUM_ARTIFACT_LISTEN`, also the `--artifact-listen` flag.
pub const ENV_ARTIFACT_LISTEN: &str = "TERRARIUM_ARTIFACT_LISTEN";
/// The default of [`ENV_ARTIFACT_LISTEN`].
pub const DEFAULT_ARTIFACT_LISTEN: &str = "0.0.0.0:8081";
/// `TERRARIUM_GIT_PROXY_LISTEN`, also the `--git-proxy-listen` flag.
pub const ENV_GIT_PROXY_LISTEN: &str = "TERRARIUM_GIT_PROXY_LISTEN";
/// The default of [`ENV_GIT_PROXY_LISTEN`].
pub const DEFAULT_GIT_PROXY_LISTEN: &str = "127.0.0.1:8084";
/// `TERRARIUM_ARTIFACT_PUBLIC_URL`: the artifact origin as a browser opens
/// it, for signed artifact URLs and the dashboard CSP.
pub const ENV_ARTIFACT_PUBLIC_URL: &str = "TERRARIUM_ARTIFACT_PUBLIC_URL";
/// `TERRARIUM_WS_QUERY_TOKEN`: `1` takes the bearer token in `?token=` on a
/// WebSocket upgrade (deprecated). The default `0` refuses it.
pub const ENV_WS_QUERY_TOKEN: &str = "TERRARIUM_WS_QUERY_TOKEN";
/// `TERRARIUM_SESSION_TTL`: the life of a dashboard session cookie.
pub const ENV_SESSION_TTL: &str = "TERRARIUM_SESSION_TTL";
/// The default of [`ENV_SESSION_TTL`].
pub const DEFAULT_SESSION_TTL: &str = "30d";
/// `TERRARIUM_CLI_SESSION_TTL`: the life of a CLI session token.
pub const ENV_CLI_SESSION_TTL: &str = "TERRARIUM_CLI_SESSION_TTL";
/// The default of [`ENV_CLI_SESSION_TTL`].
pub const DEFAULT_CLI_SESSION_TTL: &str = "90d";
/// `TERRARIUM_SESSION_MAX_AGE`: the longest life of a dashboard session
/// through its renewals, from the login.
pub const ENV_SESSION_MAX_AGE: &str = "TERRARIUM_SESSION_MAX_AGE";
/// The default of [`ENV_SESSION_MAX_AGE`].
pub const DEFAULT_SESSION_MAX_AGE: &str = "90d";

/// `TERRARIUM_HOST_DIST_DIR`: the host agent binaries for `/install/...`.
pub const ENV_HOST_DIST_DIR: &str = "TERRARIUM_HOST_DIST_DIR";
/// The image puts the static `terrarium-host` here (docker/Dockerfile).
pub const DEFAULT_HOST_DIST_DIR: &str = "/opt/terrarium/dist";
/// `TERRARIUM_HOST_DAEMON_URL`: the `daemon_url` that the host installer
/// writes. `just host::install` reads the same variable.
pub const ENV_HOST_DAEMON_URL: &str = "TERRARIUM_HOST_DAEMON_URL";

/// The settings of the host installer (`GET /install/...`,
/// `crate::api::install`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallConfig {
    /// [`ENV_HOST_DIST_DIR`].
    pub dist_dir: PathBuf,
    /// [`ENV_HOST_DAEMON_URL`], with no trailing `/`. `None`: not set.
    pub host_daemon_url: Option<String>,
    /// The host listener, for the default `daemon_url` and the pin. `None`:
    /// the listener is off. `main` sets it from the env.
    pub host_listen: Option<crate::hosts::HostListen>,
}

impl Default for InstallConfig {
    fn default() -> Self {
        Self {
            dist_dir: PathBuf::from(DEFAULT_HOST_DIST_DIR),
            host_daemon_url: None,
            host_listen: None,
        }
    }
}

/// The settings of the web side of the daemon. [`Settings::web`] makes it
/// from the flags and the env; tests make it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    pub project_pages_v2: bool,
    /// [`Settings::board_columns`]. Off in a config that a test makes
    /// directly: most tests still run the legacy flows.
    pub board_columns: bool,
    /// [`ENV_PUBLIC_URL`], with no trailing `/`. `None`: not set.
    pub public_url: Option<String>,
    /// [`ENV_ARTIFACT_PUBLIC_URL`], with no trailing `/`. `None`: not set.
    pub artifact_public_url: Option<String>,
    /// Take `?token=` on a WebSocket upgrade ([`ENV_WS_QUERY_TOKEN`]).
    pub ws_query_token: bool,
    /// The life of a session cookie ([`ENV_SESSION_TTL`]).
    pub session_ttl: std::time::Duration,
    /// The life of a CLI session token ([`ENV_CLI_SESSION_TTL`]).
    pub cli_session_ttl: std::time::Duration,
    /// The longest life of a renewed session ([`ENV_SESSION_MAX_AGE`]).
    pub session_max_age: std::time::Duration,
    /// The host installer.
    pub install: InstallConfig,
    /// The address of the git proxy ([`ENV_GIT_PROXY_LISTEN`]). `None`:
    /// no proxy (the default here, so a test daemon has none unless it
    /// asks).
    pub git_proxy_listen: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            public_url: None,
            artifact_public_url: None,
            ws_query_token: false,
            project_pages_v2: false,
            board_columns: false,
            session_ttl: std::time::Duration::from_secs(30 * 86_400),
            cli_session_ttl: std::time::Duration::from_secs(90 * 86_400),
            session_max_age: std::time::Duration::from_secs(90 * 86_400),
            install: InstallConfig::default(),
            git_proxy_listen: None,
        }
    }
}

/// A URL setting: trimmed, no trailing `/`, `None` when empty.
pub fn clean_url(v: Option<&str>) -> Option<String> {
    v.map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
}

impl WebConfig {
    /// The base of the links in operator mail: [`Self::public_url`], else
    /// [`DEFAULT_PUBLIC_URL`].
    pub fn mail_base(&self) -> &str {
        self.public_url.as_deref().unwrap_or(DEFAULT_PUBLIC_URL)
    }

    /// A dashboard link for a mail: [`Self::mail_base`] and `path` (which
    /// starts with `/`). Always absolute.
    pub fn dashboard_link(&self, path: &str) -> String {
        format!("{}{path}", self.mail_base())
    }

    /// The origin (`scheme://host[:port]`) of [`Self::public_url`].
    pub fn public_origin(&self) -> Option<String> {
        self.public_url.as_deref().map(origin_of)
    }
}

/// The origin of a URL: `https://t.example.com/x/` -> `https://t.example.com`.
pub fn origin_of(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            format!("{}://{}", scheme.to_ascii_lowercase(), authority)
        }
        None => url.to_owned(),
    }
}

/// `1`, `true`, `yes`, `on` -> true; `0`, `false`, `no`, `off`, empty -> false.
pub fn parse_switch(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!("`{other}` is not 0 or 1")),
    }
}

/// The default of `TERRARIUM_QUOTA_SAMPLE_KEEP`.
pub const DEFAULT_QUOTA_SAMPLE_KEEP: std::time::Duration =
    std::time::Duration::from_secs(48 * 3600);

/// `TERRARIUM_QUOTA_SAMPLE_KEEP`: a duration of at least 24 hours.
pub fn parse_sample_keep(s: &str) -> Result<std::time::Duration, String> {
    let d = parse_duration(s)?;
    if d < std::time::Duration::from_secs(24 * 3600) {
        return Err(format!(
            "`{}`: keep the quota samples at least 24h (the dashboard reads 24 hours)",
            s.trim()
        ));
    }
    Ok(d)
}

/// A duration: seconds (`3600`), or a number with `s`, `m`, `h`, or `d`
/// (`90m`, `12h`, `1d`). It must be more than zero.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("`{s}` is not a duration such as 3600, 90m, 12h, or 1d"))?;
    let mult = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("`{s}`: the unit must be s, m, h, or d")),
    };
    let secs = n.saturating_mul(mult);
    if secs == 0 {
        return Err("the duration must be more than zero".into());
    }
    Ok(std::time::Duration::from_secs(secs))
}

impl Settings {
    /// The times of the untouched Inbox threads (TER-95).
    pub fn thread_tuning(&self) -> crate::thread_close::Tuning {
        crate::thread_close::Tuning {
            reminder_after: self.thread_reminder,
            stale_after: self.thread_stale,
        }
    }

    /// The web settings from the flags and the env.
    pub fn web(&self) -> WebConfig {
        WebConfig {
            public_url: clean_url(self.public_url.as_deref()),
            artifact_public_url: clean_url(self.artifact_public_url.as_deref()),
            ws_query_token: self.ws_query_token,
            project_pages_v2: self.project_pages_v2,
            board_columns: self.board_columns,
            session_ttl: self.session_ttl,
            cli_session_ttl: self.cli_session_ttl,
            session_max_age: self.session_max_age,
            git_proxy_listen: Some(self.git_proxy_listen.trim().to_owned())
                .filter(|a| !a.is_empty()),
            install: InstallConfig {
                dist_dir: self.host_dist_dir.clone(),
                host_daemon_url: clean_url(self.host_daemon_url.as_deref()),
                host_listen: None,
            },
        }
    }

    /// The artifact listener: `None` when `--artifact-listen` is empty.
    pub fn artifact_listen(&self) -> Option<String> {
        Some(self.artifact_listen.trim().to_owned()).filter(|a| !a.is_empty())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("3600").unwrap().as_secs(), 3600);
        assert_eq!(parse_duration("90m").unwrap().as_secs(), 5400);
        assert_eq!(parse_duration("12h").unwrap().as_secs(), 43_200);
        assert_eq!(parse_duration("1d").unwrap().as_secs(), 86_400);
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("12x").is_err());
        assert!(parse_duration("h").is_err());
    }

    /// TER-395: the quota samples stay at least the 24 hours that the
    /// dashboard reads.
    #[test]
    fn quota_sample_keep_is_at_least_a_day() {
        assert_eq!(parse_sample_keep("48h").unwrap(), DEFAULT_QUOTA_SAMPLE_KEEP);
        assert_eq!(parse_sample_keep("1d").unwrap().as_secs(), 86_400);
        assert!(parse_sample_keep("23h").is_err());
        assert!(parse_sample_keep("0").is_err());
        let s = Settings::try_parse_from(["terrariumd"]).unwrap();
        assert_eq!(s.quota_sample_keep, DEFAULT_QUOTA_SAMPLE_KEEP);
    }

    #[test]
    fn switches_and_origins() {
        assert!(!WebConfig::default().project_pages_v2);
        assert!(!WebConfig::default().board_columns);
        assert!(
            Settings::try_parse_from(["terrariumd"])
                .unwrap()
                .web()
                .board_columns
        );
        assert!(
            !Settings::try_parse_from(["terrariumd", "--board-columns", "0"])
                .unwrap()
                .web()
                .board_columns
        );
        assert!(
            !Settings::try_parse_from(["terrariumd", "--project-pages-v2", "0"])
                .unwrap()
                .web()
                .project_pages_v2
        );
        assert!(
            Settings::try_parse_from(["terrariumd", "--project-pages-v2", "1"])
                .unwrap()
                .web()
                .project_pages_v2
        );
        assert!(Settings::try_parse_from(["terrariumd", "--project-pages-v2", "maybe"]).is_err());
        assert!(parse_switch("1").unwrap());
        assert!(!parse_switch("off").unwrap());
        assert!(parse_switch("maybe").is_err());
        assert_eq!(
            origin_of("HTTPS://t.example.com:8443/terrarium/"),
            "https://t.example.com:8443"
        );
        assert_eq!(
            clean_url(Some(" https://x/ ")).as_deref(),
            Some("https://x")
        );
        assert_eq!(clean_url(Some("")), None);
    }

    #[test]
    fn thread_times_default_and_turn_off() {
        let s = Settings::try_parse_from(["terrariumd"]).unwrap();
        assert_eq!(s.thread_tuning(), crate::thread_close::Tuning::default());
        let s = Settings::try_parse_from([
            "terrariumd",
            "--thread-reminder",
            "12h",
            "--thread-stale",
            "off",
        ])
        .unwrap();
        let t = s.thread_tuning();
        assert_eq!(t.reminder_after.as_secs(), 43_200);
        assert!(t.stale_after.is_zero());
        assert!(Settings::try_parse_from(["terrariumd", "--thread-stale", "soon"]).is_err());
    }

    #[test]
    fn flags_fill_the_web_config() {
        let s = Settings::try_parse_from([
            "terrariumd",
            "--public-url",
            "https://t.example.com/",
            "--artifact-public-url",
            "https://art.example.com",
            "--ws-query-token",
            "1",
            "--session-ttl",
            "30m",
            "--cli-session-ttl",
            "7d",
            "--artifact-listen",
            "",
        ])
        .unwrap();
        let w = s.web();
        assert_eq!(w.public_url.as_deref(), Some("https://t.example.com"));
        assert_eq!(
            w.artifact_public_url.as_deref(),
            Some("https://art.example.com")
        );
        assert!(w.ws_query_token);
        assert_eq!(w.session_ttl.as_secs(), 1800);
        assert_eq!(w.cli_session_ttl.as_secs(), 7 * 86_400);
        assert_eq!(s.artifact_listen(), None);
        assert_eq!(
            w.dashboard_link("/subscriptions"),
            "https://t.example.com/subscriptions"
        );
        let d = WebConfig::default();
        assert_eq!(d.dashboard_link("/x"), "http://localhost:8080/x");
        assert!(!d.ws_query_token);
        let defaults = Settings::try_parse_from(["terrariumd"]).unwrap().web();
        assert_eq!(defaults.session_ttl.as_secs(), 30 * 86_400);
        assert_eq!(defaults.cli_session_ttl.as_secs(), 90 * 86_400);
        assert_eq!(defaults.session_max_age.as_secs(), 90 * 86_400);
    }
}
