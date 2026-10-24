//! The command line. One tree serves both modes: the daemon decides what
//! each caller may do. The help text hides the commands that do not fit the
//! mode (`hide_for_mode`).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use terrarium_core::paths;

#[derive(Debug, Parser)]
#[command(
    name = "terrarium",
    version,
    about = "The Terrarium CLI",
    after_help = "Exit codes: 0 ok, 1 server error, 2 usage or not available, 3 refused by policy."
)]
pub struct Cli {
    /// The daemon URL, for example `http://localhost:8080`. Without it the CLI uses the Unix socket.
    #[arg(long, global = true, env = paths::ENV_URL)]
    pub url: Option<String>,

    /// The bearer token for a TCP daemon URL.
    #[arg(long, global = true, env = paths::ENV_TOKEN, hide_env_values = true)]
    pub token: Option<String>,

    /// The daemon's Unix socket.
    #[arg(long, global = true, env = paths::ENV_SOCKET)]
    pub socket: Option<PathBuf>,

    /// The project. An agent defaults to its own project.
    #[arg(long, short = 'p', global = true)]
    pub project: Option<String>,

    /// Print JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Daemon commands
    Daemon {
        #[command(subcommand)]
        command: DaemonCmd,
    },
    /// Show who this CLI acts as
    Whoami,
    /// Projects
    Project {
        #[command(subcommand)]
        command: ProjectCmd,
    },
    /// Repositories of the project
    Repo {
        #[command(subcommand)]
        command: RepoCmd,
    },
    /// Agents
    #[command(visible_alias = "agents")]
    Agent {
        #[command(subcommand)]
        command: AgentCmd,
    },
    /// Mail
    Mail {
        #[command(subcommand)]
        command: MailCmd,
    },
    /// The project inbox (`project:<name>`)
    Inbox {
        #[command(subcommand)]
        command: InboxCmd,
    },
    /// The project board, grouped by column
    Board,
    /// Cards on the board
    Card {
        #[command(subcommand)]
        command: CardCmd,
    },
    /// The project memory graph
    Mem {
        #[command(subcommand)]
        command: MemCmd,
    },
    /// The project wiki
    Wiki {
        #[command(subcommand)]
        command: WikiCmd,
    },
    /// Artifacts: files and static bundles for the user
    Artifact {
        #[command(subcommand)]
        command: ArtifactCmd,
    },
    /// Versions for review
    Version {
        #[command(subcommand)]
        command: VersionCmd,
    },
    /// Subscription usage
    Usage {
        #[command(subcommand)]
        command: Option<UsageCmd>,
    },
    /// Report this agent's own status
    Status {
        #[command(subcommand)]
        command: StatusCmd,
    },
    /// Targets
    Targets {
        #[command(subcommand)]
        command: TargetsCmd,
    },
    /// Run a command on a target; streams the output and exits with its code
    Exec {
        target: String,
        /// The working directory on the target (an absolute path)
        #[arg(long)]
        cwd: Option<String>,
        /// `KEY=VALUE` for the command's environment; repeat for more
        #[arg(long = "env", short = 'e')]
        env: Vec<String>,
        /// Kill the command after this many seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Send this CLI's stdin to the command
        #[arg(long, short = 'i')]
        stdin: bool,
        /// Run the command in a PTY
        #[arg(long, short = 't')]
        tty: bool,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Open an interactive shell on a target (Ctrl-] detaches)
    Shell { target: String },
    /// Copy one file to or from a target: `cp <src> <target>:<dst>` or `cp <target>:<src> <dst>`
    Cp { src: String, dst: String },
    /// Fleet files: many projects, agents, and targets in one file
    Fleet {
        #[command(subcommand)]
        command: FleetCmd,
    },
    /// Operator commands that an agent's role permits
    Admin {
        #[command(subcommand)]
        command: AdminCmd,
    },
    /// The audit log of privileged actions
    Audit(AuditArgs),
    /// The event log
    Events(EventsArgs),
    /// Forward a harness hook (JSON on stdin) to the runner socket
    Hook {
        /// The hook name, for example `Stop` or `UserPromptSubmit`
        event: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCmd {
    /// Check that the daemon runs; exit non-zero when it does not
    Status,
    /// Read the definition files again (sysadmin)
    Reload,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCmd {
    /// List projects
    Ls,
    /// Show a project with its agents, repos, and targets
    Show { name: Option<String> },
    /// Create a project (sysadmin)
    New {
        name: String,
        #[arg(long, short = 'd')]
        description: Option<String>,
        /// The card ID prefix, for example `TER`
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Change a project (project_admin)
    Set {
        name: Option<String>,
        #[arg(long, short = 'd')]
        description: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Delete a project (sysadmin)
    Rm {
        name: String,
        /// Needed: a delete cannot be undone
        #[arg(long)]
        confirm: bool,
    },
    /// List the repos of a project
    Repos { name: Option<String> },
    /// Attach a repo to the project (project_admin)
    RepoAdd(RepoAddArgs),
}

#[derive(Debug, Args)]
pub struct RepoAddArgs {
    pub url: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub branch: Option<String>,
    /// A key name in /state/credentials, never the secret
    #[arg(long)]
    pub credential: Option<String>,
    #[arg(long)]
    pub path: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum RepoCmd {
    /// Repositories attached to the project
    Ls,
    /// Clone or update a repo in the workspace, or on a target
    Checkout {
        repo: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        /// The directory under the workspace (default: the repo's path)
        #[arg(long)]
        path: Option<String>,
        /// Clone or fetch only this many commits
        #[arg(long)]
        depth: Option<u32>,
        /// The agent whose workspace gets the checkout (operator; an agent defaults to itself)
        #[arg(long)]
        agent: Option<String>,
    },
    /// The git credential helper: git runs `terrarium repo credential get`
    #[command(hide = true)]
    Credential {
        /// `get`, `store`, or `erase`. Only `get` answers.
        operation: String,
    },
    /// Attach a repo to the project (project_admin)
    Add(RepoAddArgs),
}

#[derive(Debug, Subcommand)]
pub enum AgentCmd {
    /// List agents
    Ls {
        /// Agents of every project the caller may read
        #[arg(long)]
        all_projects: bool,
    },
    /// Show one agent
    Show { agent: String },
    /// Create an agent from flags or from a TOML definition
    New(Box<AgentNewArgs>),
    /// Change an agent's definition
    Set(Box<AgentSetArgs>),
    /// Delete an agent
    Rm {
        agent: String,
        /// Needed when the agent has a live session
        #[arg(long)]
        confirm: bool,
    },
    /// Start an agent
    Start { agent: String },
    /// Stop an agent
    Stop { agent: String },
    /// Restart an agent
    Restart { agent: String },
    /// Type one turn into the agent
    Send {
        agent: String,
        /// The text, or `-` for stdin
        text: String,
    },
    /// Answer the agent's open question
    Answer(AnswerArgs),
    /// The agent's events
    Logs {
        agent: String,
        /// Only events after this seq or event ID
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        /// Keep printing new events
        #[arg(long, short = 'f')]
        follow: bool,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Attach this terminal to the agent's PTY. Ctrl-] detaches.
    Attach { agent: String },
    /// The agent's harness sessions, newest first
    Sessions { agent: String },
    /// The agent's roles
    Roles {
        #[command(subcommand)]
        command: RolesCmd,
    },
}

#[derive(Debug, Args)]
pub struct AgentNewArgs {
    /// The agent name. The definition file can give it.
    pub name: Option<String>,
    /// A TOML agent definition, as in `agents/<name>.toml`
    #[arg(long)]
    pub from_file: Option<PathBuf>,
    /// `claude`, `codex`, `copilot`, or `pi`
    #[arg(long)]
    pub harness: Option<String>,
    #[arg(long)]
    pub subscription: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    /// The prompt template name, for example `project-lead`
    #[arg(long)]
    pub prompt: Option<String>,
    /// `project_admin` or `sysadmin`; repeat for more
    #[arg(long = "role")]
    pub roles: Vec<String>,
    /// A target the agent may use; repeat for more
    #[arg(long = "target")]
    pub targets: Vec<String>,
    /// A mail recipient; `*` is every agent of the project; repeat for more
    #[arg(long = "mail-to")]
    pub mail_to: Vec<String>,
    /// A `project/agent` outside the project; repeat for more
    #[arg(long = "cross-project")]
    pub cross_project: Vec<String>,
    /// `always`, `on-failure`, or `never`
    #[arg(long)]
    pub restart: Option<String>,
}

#[derive(Debug, Args)]
pub struct AgentSetArgs {
    pub agent: String,
    #[arg(long)]
    pub harness: Option<String>,
    #[arg(long)]
    pub subscription: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub prompt: Option<String>,
    #[arg(long)]
    pub restart: Option<String>,
    /// The whole policy as JSON, for example `{"targets":[],"mail_to":["*"]}`
    #[arg(long)]
    pub policy: Option<String>,
}

#[derive(Debug, Args)]
pub struct AnswerArgs {
    pub agent: String,
    /// The question's request ID
    #[arg(long)]
    pub request_id: Option<String>,
    /// Pick a choice by index
    #[arg(long, conflicts_with_all = ["allow", "deny", "text"])]
    pub choice: Option<u64>,
    #[arg(long, conflicts_with_all = ["deny", "text"])]
    pub allow: bool,
    #[arg(long, conflicts_with = "text")]
    pub deny: bool,
    /// Type raw text and Enter, for a dialog the adapter does not know
    #[arg(long)]
    pub text: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum RolesCmd {
    /// Show the roles
    Get { agent: String },
    /// Replace the roles. No roles means member only.
    Set { agent: String, roles: Vec<String> },
}

#[derive(Debug, Subcommand)]
pub enum MailCmd {
    /// List mail
    Ls {
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long, short = 't')]
        thread: Option<String>,
        /// `queued`, `delivered`, `read`, `replied`, or `unread`
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        unread: bool,
    },
    /// Count my mail, unread first
    Check,
    /// Read one message, a thread, or my unread mail
    Read {
        id: Option<String>,
        #[arg(long, short = 't')]
        thread: Option<String>,
        /// Mark the messages read
        #[arg(long)]
        mark: bool,
    },
    /// Send mail
    Send {
        /// An agent, `operator`, or `project:<name>`
        to: String,
        /// The body, or `-` for stdin
        body: String,
        #[arg(long, short = 's')]
        subject: Option<String>,
        /// Post on this thread
        #[arg(long, short = 't')]
        thread: Option<String>,
        /// Send as this address (operator: `project:<name>`)
        #[arg(long)]
        from: Option<String>,
    },
    /// Reply to every participant of the message's thread
    Reply {
        id: String,
        /// The body, or `-` for stdin
        body: String,
        #[arg(long, short = 's')]
        subject: Option<String>,
    },
    /// Block until mail arrives
    Wait {
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
}

#[derive(Debug, Subcommand)]
pub enum InboxCmd {
    /// List the project inbox
    Ls {
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        state: Option<String>,
        #[arg(long, short = 't')]
        thread: Option<String>,
    },
    /// Read one inbox message
    Read {
        id: String,
        #[arg(long)]
        mark: bool,
    },
    /// Reply from the project inbox
    Reply {
        id: String,
        /// The body, or `-` for stdin
        body: String,
        #[arg(long, short = 's')]
        subject: Option<String>,
    },
    /// Send to the project inbox
    Send {
        body: String,
        #[arg(long, short = 's')]
        subject: Option<String>,
        #[arg(long, short = 't')]
        thread: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum CardCmd {
    /// List cards
    Ls {
        #[arg(long, short = 'c')]
        column: Option<String>,
        /// Cards assigned to me
        #[arg(long, conflicts_with = "assignee")]
        mine: bool,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        unassigned: bool,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        archived: bool,
    },
    /// Body, comments, links, and dependencies
    Show { id: String },
    /// Create a card
    New(Box<CardNewArgs>),
    /// Change a card
    Edit(Box<CardEditArgs>),
    /// Assign to self and move to in_progress
    Claim { id: String },
    /// Move a card to a column
    Move {
        id: String,
        column: String,
        #[arg(long)]
        position: Option<u64>,
        /// The move comment; `blocked` needs one
        #[arg(long, short = 'm')]
        comment: Option<String>,
    },
    /// Assign a card: an agent, `operator`, `me`, or `none`
    Assign { id: String, agent: String },
    /// Comment on a card
    Comment {
        id: String,
        /// The text, or `-` for stdin
        text: String,
    },
    /// Link a mail thread, branch, card, or URL
    Link {
        id: String,
        #[arg(long, group = "link")]
        thread: Option<String>,
        #[arg(long, group = "link")]
        branch: Option<String>,
        #[arg(long, group = "link")]
        card: Option<String>,
        #[arg(long = "link-url", group = "link")]
        link_url: Option<String>,
    },
    /// Archive a card
    Rm { id: String },
    /// The card's history
    Events { id: String },
    /// Block until a card is assigned to me or moves
    Wait {
        #[arg(long)]
        mine: bool,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
}

#[derive(Debug, Args)]
pub struct CardNewArgs {
    /// The title. `--from-mail` can give it.
    pub title: Option<String>,
    /// The body, or `-` for stdin
    #[arg(long, short = 'b')]
    pub body: Option<String>,
    #[arg(long, short = 'c')]
    pub column: Option<String>,
    #[arg(long = "label")]
    pub labels: Vec<String>,
    #[arg(long = "depends-on")]
    pub depends_on: Vec<String>,
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long)]
    pub repo: Option<String>,
    #[arg(long)]
    pub branch: Option<String>,
    /// Make the card from a message: subject, body, and thread
    #[arg(long)]
    pub from_mail: Option<String>,
    /// A comment, needed for `--column blocked`
    #[arg(long, short = 'm')]
    pub comment: Option<String>,
}

#[derive(Debug, Args)]
pub struct CardEditArgs {
    pub id: String,
    #[arg(long)]
    pub title: Option<String>,
    /// The body, or `-` for stdin
    #[arg(long, short = 'b')]
    pub body: Option<String>,
    /// Replace the labels; repeat for more
    #[arg(long = "label")]
    pub labels: Vec<String>,
    /// Replace the dependencies; repeat for more
    #[arg(long = "depends-on")]
    pub depends_on: Vec<String>,
    #[arg(long)]
    pub repo: Option<String>,
    #[arg(long)]
    pub branch: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum MemCmd {
    /// Full-text search, newest first on ties
    Search {
        query: String,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Show superseded memories too
        #[arg(long)]
        all: bool,
    },
    /// Body, sources, and linked memories to depth
    Show {
        id: String,
        #[arg(long, default_value_t = 1)]
        depth: u32,
    },
    /// Write a memory
    New(Box<MemNewArgs>),
    /// Link two memories
    Link {
        a: String,
        b: String,
        relation: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Remove one of your links
    Unlink {
        a: String,
        b: String,
        relation: String,
    },
    /// Change one of your memories
    Edit {
        id: String,
        #[arg(long, short = 'b')]
        body: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Replace the tags; repeat for more
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Mark `old` as superseded by `new`
    Supersede { old: String, new: String },
    /// The memories linked to one memory
    Neighbors {
        id: String,
        #[arg(long)]
        relation: Option<String>,
    },
    /// List memories
    Ls {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        mine: bool,
        #[arg(long, default_value_t = 20)]
        recent: usize,
    },
    /// Delete a memory (operator)
    Rm { id: String },
}

#[derive(Debug, Args)]
pub struct MemNewArgs {
    /// Title it as a claim
    pub title: String,
    /// The body, or `-` for stdin
    #[arg(long, short = 'b')]
    pub body: Option<String>,
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// `card:TER-42`, `thread:<id>`, `path:<p>`, or `commit:<sha>`
    #[arg(long = "source")]
    pub sources: Vec<String>,
    /// `<id>:<relation>`
    #[arg(long = "link")]
    pub links: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum WikiCmd {
    /// The page tree
    Ls { path: Option<String> },
    /// A page's Markdown body
    Show {
        page: String,
        #[arg(long)]
        rev: Option<u64>,
    },
    /// Search the wiki
    Search { query: String },
    /// Create a page; prints the preview URL
    New {
        page: String,
        #[arg(long, short = 'b')]
        body: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(short = 'm', long = "summary")]
        summary: Option<String>,
    },
    /// Replace a page; refused if the base revision is stale
    Edit {
        page: String,
        #[arg(long, short = 'b')]
        body: String,
        #[arg(long)]
        base: u64,
        #[arg(short = 'm', long = "summary")]
        summary: Option<String>,
    },
    /// Replace one section by its heading
    Section {
        page: String,
        heading: String,
        #[arg(long, short = 'b')]
        body: String,
        #[arg(long)]
        base: Option<u64>,
        #[arg(short = 'm', long = "summary")]
        summary: Option<String>,
    },
    /// Append to a page
    Append {
        page: String,
        #[arg(long, short = 'b')]
        body: String,
        #[arg(short = 'm', long = "summary")]
        summary: Option<String>,
    },
    /// The revisions of a page
    History { page: String },
    /// The diff between two revisions
    Diff { page: String, a: u64, b: u64 },
    /// Move a page (admins)
    Mv { old: String, new: String },
    /// Delete a page; history stays (admins)
    Rm { page: String },
    /// Restore an old revision as a new one
    Restore { page: String, rev: u64 },
    /// Pages that link to this page
    Backlinks { page: String },
}

#[derive(Debug, Subcommand)]
pub enum ArtifactCmd {
    /// Upload a file, or a directory as a static bundle; prints the preview URL
    Push {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, group = "attach")]
        page: Option<String>,
        #[arg(long, group = "attach")]
        version: Option<String>,
        #[arg(long, group = "attach")]
        card: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// List artifacts
    Ls {
        #[arg(long, group = "attach")]
        page: Option<String>,
        #[arg(long, group = "attach")]
        version: Option<String>,
        #[arg(long, group = "attach")]
        card: Option<String>,
    },
    /// The stable preview URL
    Url { id: String },
    /// Delete an artifact (own and unattached only)
    Rm { id: String },
}

#[derive(Debug, Subcommand)]
pub enum VersionCmd {
    /// Create a version
    New {
        name: String,
        /// The notes, or `-` for stdin
        #[arg(long)]
        notes: Option<String>,
        #[arg(long, requires = "git_ref")]
        repo: Option<String>,
        #[arg(long = "ref")]
        git_ref: Option<String>,
        #[arg(long = "card")]
        cards: Vec<String>,
    },
    /// List versions
    Ls,
    /// Show a version
    Show { name: String },
    /// Attach an artifact
    Attach { name: String, artifact: String },
    /// draft -> in_review; mails the operator
    Submit { name: String },
    /// Block until approved or rejected
    Wait {
        name: String,
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
    },
    /// approved -> released (admins)
    Release { name: String },
    /// Approve a version (operator)
    Approve {
        name: String,
        #[arg(long, short = 'm')]
        comment: Option<String>,
    },
    /// Reject a version (operator)
    Reject {
        name: String,
        #[arg(long, short = 'm')]
        comment: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum UsageCmd {
    /// Per-agent breakdown for the project
    Project,
    /// All plans, remaining per window
    Subscriptions,
}

#[derive(Debug, Subcommand)]
pub enum StatusCmd {
    /// Report `idle`, `working`, or `blocked`
    Set {
        status: String,
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum TargetsCmd {
    /// Targets this caller may use
    Ls,
}

#[derive(Debug, Subcommand)]
pub enum AdminCmd {
    /// Agent commands, as `terrarium agent`
    Agent {
        #[command(subcommand)]
        command: AgentCmd,
    },
    /// Project commands, as `terrarium project`
    Project {
        #[command(subcommand)]
        command: ProjectCmd,
    },
    /// Grant or revoke roles
    Role {
        #[command(subcommand)]
        command: RoleCmd,
    },
    /// Targets
    Target {
        #[command(subcommand)]
        command: AdminTargetCmd,
    },
    /// Host agents
    Host {
        #[command(subcommand)]
        command: HostCmd,
    },
    /// Fleet files, as `terrarium fleet`
    Fleet {
        #[command(subcommand)]
        command: FleetCmd,
    },
    /// The audit log
    Audit(AuditArgs),
}

#[derive(Debug, Subcommand)]
pub enum FleetCmd {
    /// Check a fleet file and list its objects; exit 2 on errors
    Validate {
        /// The fleet file, or `-` for stdin
        file: String,
        /// Check the file only, without the daemon (no references, no prompts)
        #[arg(long)]
        offline: bool,
    },
    /// Show what an apply would create, update, and delete
    Plan {
        /// The fleet file, or `-` for stdin
        file: String,
        /// Plan the removal of objects that the file no longer declares
        #[arg(long)]
        prune: bool,
    },
    /// Make the daemon match the fleet file
    Apply {
        /// The fleet file, or `-` for stdin
        file: String,
        /// Delete the agents and targets that the file no longer declares
        #[arg(long)]
        prune: bool,
        /// Needed when --prune deletes an agent with a live session
        #[arg(long)]
        confirm: bool,
        /// Start the agents with `start = true` that do not run
        #[arg(long)]
        start: bool,
    },
    /// List the applied fleets
    Ls,
    /// Show a fleet, its last applied file, and its plan now (drift shows there)
    Show { name: String },
    /// Write a fleet file from a project as it is now
    Export {
        project: String,
        /// The fleet name (default: the project name)
        #[arg(long)]
        name: Option<String>,
        /// Write to this file, not stdout
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum RoleCmd {
    /// Add a role
    Grant { agent: String, role: String },
    /// Remove a role
    Revoke { agent: String, role: String },
    /// Show the roles
    Ls { agent: String },
}

#[derive(Debug, Subcommand)]
pub enum AdminTargetCmd {
    /// Define a target (sysadmin for a global target)
    New {
        name: String,
        /// `host`, `ssh`, `docker`, or `vmlab`
        #[arg(long)]
        kind: String,
        /// `ssh`: `user@host` or `host`
        #[arg(long)]
        address: Option<String>,
        /// `ssh`: the port
        #[arg(long)]
        port: Option<u16>,
        /// `ssh`: the ssh_key credential name
        #[arg(long)]
        credential: Option<String>,
        /// The host agent, for `host`, `docker`, and `vmlab`
        #[arg(long)]
        host: Option<String>,
        /// `docker`: the image to run
        #[arg(long)]
        image: Option<String>,
        /// `docker`: an existing container
        #[arg(long)]
        container: Option<String>,
        /// `vmlab`: the lab directory (holds `vmlab.wcl`)
        #[arg(long)]
        lab: Option<String>,
        /// `vmlab`: the machine in the lab
        #[arg(long)]
        machine: Option<String>,
        /// Made on agent start and removed on agent stop
        #[arg(long)]
        ephemeral: bool,
        /// Keep an ephemeral target after the agent stops
        #[arg(long)]
        keep: bool,
        /// Make a global target instead of a project target
        #[arg(long)]
        global: bool,
    },
    /// Remove a target
    Rm { name: String },
    /// Run a lifecycle verb: up, down, snapshot, restore, or status
    Verb {
        name: String,
        /// `up`, `down`, `snapshot`, `restore`, or `status`
        verb: String,
        /// The snapshot name for `snapshot` and `restore`
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// List targets
    Ls,
}

#[derive(Debug, Subcommand)]
pub enum HostCmd {
    /// List host agents
    Ls,
    /// Register a host agent; prints its token once on stdout
    New { name: String },
    /// Revoke a host agent's token
    Rm { name: String },
}

#[derive(Debug, Args)]
pub struct AuditArgs {
    /// Rows at or after this RFC 3339 time
    #[arg(long)]
    pub since: Option<String>,
    /// `operator`, `system`, or `agent:<ulid>`
    #[arg(long)]
    pub actor: Option<String>,
    /// For example `agent.start`
    #[arg(long)]
    pub action: Option<String>,
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct EventsArgs {
    /// Keep printing new events
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// Only events after this seq or event ID
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub kind: Option<String>,
    /// An agent key or ULID
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
}

/// Commands to hide from the help of each mode. They still parse and run.
pub fn hide_for_mode(cmd: clap::Command, agent_mode: bool) -> clap::Command {
    let hidden: &[&str] = if agent_mode {
        &["daemon", "events", "audit", "hook"]
    } else {
        &["admin", "status", "exec", "shell", "cp", "hook"]
    };
    hidden
        .iter()
        .fold(cmd, |c, name| c.mut_subcommand(*name, |s| s.hide(true)))
}
