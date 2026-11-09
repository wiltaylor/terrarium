//! Env var names that steer a process: the loader and the C library, a
//! shell, the sandbox and the daemon, a language runtime, the proxies and
//! CA files, a harness and its account. A grant cannot set them
//! (`terrariumd` `secrets::grants::check_env_name`, docs/api.md §5.2), and
//! the host agent never puts a command env value with such a name into the
//! env of its own `docker` CLI (`terrarium_host::docker::steers_the_cli`,
//! TER-490). One list, so the two cannot drift.

/// Env var names that a grant cannot set (docs/api.md §5.2, "The name
/// rule"): the loader and the C library, the shell, the sandbox and the
/// daemon, the locale and the terminal, the proxy and CA vars, and the
/// vars that a harness adapter or an account sets.
pub const DENIED_ENV: &[&str] = &[
    // The sandbox, the daemon, the session.
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TERMINFO",
    "TERMINFO_DIRS",
    "TERMCAP",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TZDIR",
    "PWD",
    "OLDPWD",
    "DISPLAY",
    "MAIL",
    "MAILPATH",
    // The loader and the C library.
    "GCONV_PATH",
    "GLIBC_TUNABLES",
    "LOCPATH",
    "NLSPATH",
    "HOSTALIASES",
    "RES_OPTIONS",
    "LOCALDOMAIN",
    "TMPDIR",
    // The shell.
    "IFS",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PS1",
    "PS2",
    "PS4",
    "PROMPT_COMMAND",
    "CDPATH",
    "GLOBIGNORE",
    "HISTFILE",
    "INPUTRC",
    "POSIXLY_CORRECT",
    // Programs that other programs start.
    "EDITOR",
    "VISUAL",
    "PAGER",
    "MANPAGER",
    "LESSOPEN",
    "LESSCLOSE",
    "BROWSER",
    // Language runtimes.
    "PERLLIB",
    "CLASSPATH",
    "JDK_JAVA_OPTIONS",
    "GOFLAGS",
    "GOPATH",
    "GOROOT",
    "GOPROXY",
    "GOENV",
    "GOTOOLCHAIN",
    "GOINSECURE",
    "GOPRIVATE",
    "GONOSUMDB",
    "GONOPROXY",
    "VIRTUAL_ENV",
    "TCLLIBPATH",
    "PHPRC",
    // Proxies and CA files.
    "NO_PROXY",
    "CURL_CA_BUNDLE",
    "CURL_HOME",
    "REQUESTS_CA_BUNDLE",
    "SSLKEYLOGFILE",
    "WGETRC",
    "GNUPGHOME",
    // The harness adapters and the accounts.
    "IS_SANDBOX",
    "ENABLE_CLAUDEAI_MCP_SERVERS",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
    "XAI_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "MISTRAL_API_KEY",
    "ZAI_API_KEY",
    "KIMI_API_KEY",
    "MOONSHOT_API_KEY",
];

/// Prefixes of the env var names that a grant cannot set.
pub const DENIED_ENV_PREFIXES: &[&str] = &[
    // The loader, the C library, the shell, the locale.
    "LD_",
    "MALLOC_",
    "BASH_",
    "LC_",
    // git: the config, the dirs, the ssh command, the identity.
    "GIT_",
    // The sandbox and the daemon.
    "TERRARIUM_",
    "XDG_",
    "SSH_",
    "WAYLAND_",
    // Language runtimes and package managers.
    "NODE_",
    "NPM_CONFIG_",
    "YARN_",
    "BUN_",
    "DENO_",
    "PYTHON",
    "PIP_",
    "UV_",
    "CONDA_",
    "PERL5",
    "RUBY",
    "GEM_",
    "BUNDLE_",
    "JAVA_",
    "DOTNET_",
    "COREHOST_",
    "CGO_",
    "CARGO_",
    "RUSTC",
    "RUSTDOC",
    "RUSTFLAGS",
    "RUSTUP_",
    "LUA_",
    // CA files.
    "SSL_CERT_",
    // The harnesses and their accounts.
    "ANTHROPIC_",
    "CLAUDE_",
    "OPENAI_",
    "CODEX_",
    "COPILOT_",
    "PI_",
    "DISABLE_",
];

/// Suffixes of the env var names that a grant cannot set: the proxies
/// (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `FTP_PROXY`).
pub const DENIED_ENV_SUFFIXES: &[&str] = &["_PROXY"];

/// Is `name` in the deny set: [`DENIED_ENV`], a prefix of
/// [`DENIED_ENV_PREFIXES`], a suffix of [`DENIED_ENV_SUFFIXES`], or the
/// home var of a harness? Exact case.
pub fn denied(name: &str) -> bool {
    DENIED_ENV.contains(&name)
        || DENIED_ENV_PREFIXES.iter().any(|p| name.starts_with(p))
        || DENIED_ENV_SUFFIXES.iter().any(|p| name.ends_with(p))
        || crate::HarnessId::ALL.iter().any(|h| h.home_env() == name)
}
