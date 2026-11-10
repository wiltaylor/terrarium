// The name rule of an env var of a secret grant (docs/api.md §5.2, "The name
// rule"), after `check_env_name` in the daemon's `secrets/grants.rs`. The
// daemon checks it too; the grant form and the mock use this copy.

/** The shape: an upper-case letter, then upper-case letters, digits, or `_`; at most 128. */
export const ENV_NAME_RE = /^[A-Z][A-Z0-9_]{0,127}$/;

const DENIED = new Set([
  "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "TERMINFO", "TERMINFO_DIRS", "TERMCAP",
  "LANG", "LANGUAGE", "TZ", "TZDIR", "PWD", "OLDPWD", "DISPLAY", "MAIL", "MAILPATH",
  "GCONV_PATH", "GLIBC_TUNABLES", "LOCPATH", "NLSPATH", "HOSTALIASES", "RES_OPTIONS", "LOCALDOMAIN", "TMPDIR",
  "IFS", "ENV", "SHELLOPTS", "BASHOPTS", "PS1", "PS2", "PS4", "PROMPT_COMMAND", "CDPATH", "GLOBIGNORE",
  "HISTFILE", "INPUTRC", "POSIXLY_CORRECT",
  "EDITOR", "VISUAL", "PAGER", "MANPAGER", "LESSOPEN", "LESSCLOSE", "BROWSER",
  "PERLLIB", "CLASSPATH", "JDK_JAVA_OPTIONS", "GOFLAGS", "GOPATH", "GOROOT", "GOPROXY", "GOENV",
  "GOTOOLCHAIN", "GOINSECURE", "GOPRIVATE", "GONOSUMDB", "GONOPROXY", "VIRTUAL_ENV", "TCLLIBPATH", "PHPRC",
  "NO_PROXY", "CURL_CA_BUNDLE", "CURL_HOME", "REQUESTS_CA_BUNDLE", "SSLKEYLOGFILE", "WGETRC", "GNUPGHOME",
  "IS_SANDBOX", "ENABLE_CLAUDEAI_MCP_SERVERS", "GEMINI_API_KEY", "OPENROUTER_API_KEY", "XAI_API_KEY",
  "GROQ_API_KEY", "DEEPSEEK_API_KEY", "MISTRAL_API_KEY", "ZAI_API_KEY", "KIMI_API_KEY", "MOONSHOT_API_KEY",
  "CLAUDE_CONFIG_DIR", "CODEX_HOME", "COPILOT_HOME", "PI_CODING_AGENT_DIR",
]);

const DENIED_PREFIX = [
  "LD_", "MALLOC_", "BASH_", "LC_", "GIT_", "TERRARIUM_", "XDG_", "SSH_", "WAYLAND_",
  "NODE_", "NPM_CONFIG_", "YARN_", "BUN_", "DENO_", "PYTHON", "PIP_", "UV_", "CONDA_", "PERL5", "RUBY",
  "GEM_", "BUNDLE_", "JAVA_", "DOTNET_", "COREHOST_", "CGO_", "CARGO_", "RUSTC", "RUSTDOC", "RUSTFLAGS",
  "RUSTUP_", "LUA_", "SSL_CERT_", "OPENSSL_", "KRB5", "DBUS_",
  "ANTHROPIC_", "CLAUDE_", "OPENAI_", "CODEX_", "COPILOT_", "PI_", "DISABLE_",
];

/** Why the daemon refuses `env` as a final env var name, or null. */
export function envNameError(env: string): string | null {
  if (!ENV_NAME_RE.test(env)) return "An env var name is an upper-case letter, then upper-case letters, digits, or _ (at most 128).";
  if (DENIED.has(env) || DENIED_PREFIX.some((p) => env.startsWith(p)) || env.endsWith("_PROXY"))
    return `The sandbox, the loader, a shell, a language runtime, a proxy, a library that ssh loads, or a harness reads or sets ${env}.`;
  return null;
}
