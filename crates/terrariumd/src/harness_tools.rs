//! The harness tools dir: the harnesses live on a persistent volume, not in
//! the image (docs/getting-started.md §7, docs/CONVENTIONS.md).
//!
//! The tools dir (`TERRARIUM_TOOLS_DIR`, `/opt/terrarium/tools` in the
//! image, `./tools` outside Docker) is the npm global prefix. npm puts each
//! package in `<dir>/lib/node_modules/<package>` and its command in
//! `<dir>/bin`, which comes first on `PATH`. The sandbox binds the dir
//! read-only (`crate::sandbox`).
//!
//! Two operations share this one implementation:
//!
//! - [`ToolsConfig::install`] (`terrariumd harness install`, run by the
//!   entrypoint and by the daemon at each start): install each pin of
//!   `harnesses.toml` that is missing. A package that is present stays at
//!   its version, so an update survives a restart. Then install the Copilot
//!   prompt extension into the Copilot home when its marker changed.
//! - [`ToolsConfig::update`] (`POST /api/harnesses/{name}/update`): run
//!   `npm install -g <package>@<version>` in place and read the new version.
//!
//! Both write the manifest `<dir>/terrarium-manifest.json`:
//! `{"<name>": {"package", "version", "installed_at"}}`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use terrarium_core::HarnessId;

/// The env var of the tools dir. Empty: no tools dir; the harnesses come
/// from `PATH` as they are.
pub const ENV_TOOLS_DIR: &str = "TERRARIUM_TOOLS_DIR";
/// The default tools dir, relative to the daemon's cwd.
pub const DEFAULT_TOOLS_DIR: &str = "tools";
/// The manifest file in the tools dir.
pub const MANIFEST: &str = "terrarium-manifest.json";
/// The pins file in the config dir (`/etc/terrarium/harnesses.toml` in the
/// image; the repo root for `just daemon::run`).
pub const PINS_FILE: &str = "harnesses.toml";
/// The marker of the Copilot prompt extension in the Copilot home.
pub const EXTENSION_MARKER: &str = ".terrarium-extension-version";
/// The name of the Copilot prompt extension in `harnesses.toml`.
pub const COPILOT_PROMPT: &str = "copilot-prompt";
/// The limit of one npm run.
pub const NPM_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One package to install: a `[harness.<name>]` or `[extension.<name>]`
/// table of `harnesses.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pin {
    /// The harness name, or the extension name (`copilot-prompt`).
    pub name: String,
    pub package: String,
    /// The version of the first install.
    pub version: String,
    /// The command in `<dir>/bin`.
    pub binary: String,
    /// The harness: itself, or the one that uses the extension.
    pub harness: HarnessId,
    pub extension: bool,
}

/// The pins compiled into this binary (`terrarium_core::harnesses`).
pub fn compiled_pins() -> Vec<Pin> {
    let mut out: Vec<Pin> = terrarium_core::harnesses::PINS
        .iter()
        .map(|p| Pin {
            name: p.harness.to_string(),
            package: p.package.to_owned(),
            version: p.version.to_owned(),
            binary: p.binary.to_owned(),
            harness: p.harness,
            extension: false,
        })
        .collect();
    out.extend(terrarium_core::harnesses::EXTENSIONS.iter().map(|e| Pin {
        name: e.name.to_owned(),
        package: e.package.to_owned(),
        version: e.version.to_owned(),
        binary: e.binary.to_owned(),
        harness: e.harness,
        extension: true,
    }));
    out
}

#[derive(Deserialize)]
struct PinsFile {
    #[serde(default)]
    harness: BTreeMap<String, HarnessTable>,
    #[serde(default)]
    extension: BTreeMap<String, ExtensionTable>,
}

#[derive(Deserialize)]
struct HarnessTable {
    package: String,
    version: String,
    binary: String,
}

#[derive(Deserialize)]
struct ExtensionTable {
    package: String,
    version: String,
    binary: String,
    #[serde(rename = "for")]
    for_harness: String,
}

/// Read the pins from a `harnesses.toml`.
pub fn parse_pins(text: &str) -> Result<Vec<Pin>, String> {
    let file: PinsFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (name, h) in file.harness {
        let harness: HarnessId = name.parse()?;
        out.push(Pin {
            name,
            package: h.package,
            version: h.version,
            binary: h.binary,
            harness,
            extension: false,
        });
    }
    for (name, e) in file.extension {
        out.push(Pin {
            name,
            package: e.package,
            version: e.version,
            binary: e.binary,
            harness: e.for_harness.parse()?,
            extension: true,
        });
    }
    if out.is_empty() {
        return Err("no [harness.<name>] table".into());
    }
    Ok(out)
}

/// The pins of `file` when it exists, else the compiled pins. The second
/// value names the source, for the log.
pub fn load_pins(file: &Path) -> (Vec<Pin>, String) {
    match std::fs::read_to_string(file) {
        Ok(text) => match parse_pins(&text) {
            Ok(p) => (p, file.display().to_string()),
            Err(e) => {
                tracing::warn!(file = %file.display(), "cannot read the pins ({e}); using the pins built into terrariumd");
                (compiled_pins(), "built in".into())
            }
        },
        Err(_) => (compiled_pins(), "built in".into()),
    }
}

/// One manifest row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub package: String,
    pub version: String,
    pub installed_at: DateTime<Utc>,
}

/// The manifest: one row for each installed package, by pin name.
pub type Manifest = BTreeMap<String, ManifestEntry>;

/// True for a version the update accepts: `latest`, or a semver version.
/// Nothing else reaches the npm command line.
pub fn valid_version(v: &str) -> bool {
    v == "latest" || semver::Version::parse(v).is_ok()
}

/// One npm run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmRun {
    /// The exit code; `None` when npm did not start, was killed, or timed out.
    pub code: Option<i32>,
    /// stdout, then stderr.
    pub output: String,
    /// Why the run did not give an exit code.
    pub error: Option<String>,
}

impl NpmRun {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Why the run failed, for a message.
    fn failure(&self, spec: &str) -> String {
        match (&self.error, self.code) {
            (Some(e), _) => format!("npm install -g {spec}: {e}"),
            (None, Some(c)) => format!("npm install -g {spec} failed (exit {c})"),
            (None, None) => format!("npm install -g {spec} was killed"),
        }
    }
}

/// What [`ToolsConfig::install`] did with one pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallItem {
    pub name: String,
    pub package: String,
    /// The version in the tools dir after the step; `None` on a failure.
    pub version: Option<String>,
    /// `present` (no change), `installed`, or `failed`.
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output: String,
}

/// The result of [`ToolsConfig::install`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallReport {
    pub tools_dir: PathBuf,
    pub items: Vec<InstallItem>,
    /// What happened to the Copilot prompt extension.
    pub extension: String,
}

impl InstallReport {
    pub fn failed(&self) -> usize {
        self.items.iter().filter(|i| i.action == "failed").count()
    }
}

/// A done update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Updated {
    pub name: String,
    pub package: String,
    /// The version before; `None` when the package was not installed.
    pub from: Option<String>,
    pub to: String,
    pub output: String,
}

/// A failed update: the message and npm's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFailure {
    pub message: String,
    pub output: String,
    pub from: Option<String>,
}

/// The tools dir and how to fill it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsConfig {
    /// The tools dir, absolute.
    pub dir: PathBuf,
    /// The npm program.
    pub npm: PathBuf,
    pub pins: Vec<Pin>,
    /// The Copilot home, for the prompt extension.
    pub copilot_home: PathBuf,
    /// The limit of one npm run.
    pub timeout: Duration,
}

impl ToolsConfig {
    pub fn new(dir: PathBuf, pins: Vec<Pin>, copilot_home: PathBuf) -> Self {
        Self {
            dir,
            npm: PathBuf::from("npm"),
            pins,
            copilot_home,
            timeout: NPM_TIMEOUT,
        }
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.dir.join("bin")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST)
    }

    pub fn pin(&self, name: &str) -> Option<&Pin> {
        self.pins.iter().find(|p| p.name == name)
    }

    /// The manifest; empty when it is missing or does not parse.
    pub fn read_manifest(&self) -> Manifest {
        std::fs::read_to_string(self.manifest_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn write_manifest(&self, m: &Manifest) -> std::io::Result<()> {
        let tmp = self.dir.join(format!("{MANIFEST}.tmp"));
        let text = serde_json::to_string_pretty(m).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, text + "\n")?;
        std::fs::rename(tmp, self.manifest_path())
    }

    /// The version in `lib/node_modules/<package>/package.json`.
    pub fn package_version(&self, pin: &Pin) -> Option<String> {
        let path = self
            .dir
            .join("lib/node_modules")
            .join(&pin.package)
            .join("package.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        v["version"].as_str().map(str::to_owned)
    }

    /// The installed version, when the package and its command are there.
    pub fn present(&self, pin: &Pin) -> Option<String> {
        if !self.bin_dir().join(&pin.binary).exists() {
            return None;
        }
        self.package_version(pin)
    }

    /// `<dir>/bin/<binary> --version` for a harness; the package version
    /// for an extension (its command has no `--version`).
    pub async fn verify(&self, pin: &Pin) -> Result<String, String> {
        if pin.extension {
            return self
                .present(pin)
                .ok_or_else(|| format!("{} is not in {}", pin.package, self.dir.display()));
        }
        let bin = self.bin_dir().join(&pin.binary);
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new(&bin)
                .arg("--version")
                .env("PATH", self.path_env())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| format!("{} --version did not answer in 60 s", bin.display()))?
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
        let text = String::from_utf8_lossy(&out.stdout);
        terrarium_core::adapter::parse_version_output(&text)
            .map(|v| v.to_string())
            .map_err(|e| format!("{} --version: {e}", bin.display()))
    }

    /// `PATH` with the tools `bin/` first, for npm and the commands.
    fn path_env(&self) -> String {
        let rest = std::env::var("PATH").unwrap_or_default();
        prepend_path(&self.bin_dir(), &rest)
    }

    /// `npm install -g <specs>` into the tools dir.
    pub async fn npm_install(&self, specs: &[String]) -> NpmRun {
        let mut cmd = tokio::process::Command::new(&self.npm);
        cmd.args(["install", "-g", "--no-audit", "--no-fund", "--prefix"])
            .arg(&self.dir)
            .args(specs)
            .env("NPM_CONFIG_PREFIX", &self.dir)
            .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
            .env("PATH", self.path_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return NpmRun {
                    code: None,
                    output: String::new(),
                    error: Some(format!("cannot run {}: {e}", self.npm.display())),
                };
            }
        };
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Err(_) => NpmRun {
                code: None,
                output: String::new(),
                error: Some(format!(
                    "npm did not finish in {} s; it was killed",
                    self.timeout.as_secs()
                )),
            },
            Ok(Err(e)) => NpmRun {
                code: None,
                output: String::new(),
                error: Some(format!("npm failed: {e}")),
            },
            Ok(Ok(out)) => {
                let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
                output.push_str(&String::from_utf8_lossy(&out.stderr));
                NpmRun {
                    code: out.status.code(),
                    output,
                    error: None,
                }
            }
        }
    }

    /// Install each missing pin; keep each present package at its version.
    /// Then the Copilot prompt extension. Writes the manifest.
    pub async fn install(&self) -> InstallReport {
        let mut items = Vec::new();
        if let Err(e) = std::fs::create_dir_all(self.bin_dir()) {
            for p in &self.pins {
                items.push(InstallItem {
                    name: p.name.clone(),
                    package: p.package.clone(),
                    version: None,
                    action: "failed".into(),
                    error: Some(format!("cannot make {}: {e}", self.dir.display())),
                    output: String::new(),
                });
            }
            return InstallReport {
                tools_dir: self.dir.clone(),
                items,
                extension: "skipped".into(),
            };
        }
        let mut manifest = self.read_manifest();
        let before = manifest.clone();
        for pin in &self.pins {
            if let Some(v) = self.present(pin) {
                if manifest.get(&pin.name).is_none_or(|e| e.version != v) {
                    manifest.insert(
                        pin.name.clone(),
                        ManifestEntry {
                            package: pin.package.clone(),
                            version: v.clone(),
                            installed_at: Utc::now(),
                        },
                    );
                }
                items.push(InstallItem {
                    name: pin.name.clone(),
                    package: pin.package.clone(),
                    version: Some(v),
                    action: "present".into(),
                    error: None,
                    output: String::new(),
                });
                continue;
            }
            let spec = format!("{}@{}", pin.package, pin.version);
            tracing::info!(package = %spec, dir = %self.dir.display(), "installing a harness");
            let run = self.npm_install(std::slice::from_ref(&spec)).await;
            let result = if run.ok() {
                self.verify(pin).await
            } else {
                Err(run.failure(&spec))
            };
            match result {
                Ok(v) => {
                    manifest.insert(
                        pin.name.clone(),
                        ManifestEntry {
                            package: pin.package.clone(),
                            version: v.clone(),
                            installed_at: Utc::now(),
                        },
                    );
                    items.push(InstallItem {
                        name: pin.name.clone(),
                        package: pin.package.clone(),
                        version: Some(v),
                        action: "installed".into(),
                        error: None,
                        output: run.output,
                    });
                }
                Err(e) => {
                    manifest.remove(&pin.name);
                    items.push(InstallItem {
                        name: pin.name.clone(),
                        package: pin.package.clone(),
                        version: None,
                        action: "failed".into(),
                        error: Some(e),
                        output: run.output,
                    });
                }
            }
        }
        if manifest != before
            && let Err(e) = self.write_manifest(&manifest)
        {
            tracing::warn!("cannot write {}: {e}", self.manifest_path().display());
        }
        let extension = match self.ensure_extension(false).await {
            Ok(s) => s,
            Err(e) => format!("failed: {e}"),
        };
        InstallReport {
            tools_dir: self.dir.clone(),
            items,
            extension,
        }
    }

    /// `npm install -g <package>@<version>` for the pin `name`, then the new
    /// version (`--version` for a harness) and the manifest. A Copilot or
    /// extension update also reinstalls the prompt extension.
    pub async fn update(&self, name: &str, version: &str) -> Result<Updated, UpdateFailure> {
        let fail = |message: String, output: String, from: Option<String>| UpdateFailure {
            message,
            output,
            from,
        };
        let Some(pin) = self.pin(name) else {
            return Err(fail(
                format!("unknown harness `{name}`"),
                String::new(),
                None,
            ));
        };
        if !valid_version(version) {
            return Err(fail(
                format!("`{version}` is not `latest` or a version like 1.2.3"),
                String::new(),
                None,
            ));
        }
        let from = self.present(pin);
        if let Err(e) = std::fs::create_dir_all(self.bin_dir()) {
            return Err(fail(
                format!("cannot make {}: {e}", self.dir.display()),
                String::new(),
                from,
            ));
        }
        let spec = format!("{}@{version}", pin.package);
        tracing::info!(package = %spec, from = from.as_deref().unwrap_or("-"), "updating a harness");
        let run = self.npm_install(std::slice::from_ref(&spec)).await;
        if !run.ok() {
            return Err(fail(run.failure(&spec), run.output, from));
        }
        let to = match self.verify(pin).await {
            Ok(v) => v,
            Err(e) => {
                return Err(fail(
                    format!("npm installed {spec}, but {e}"),
                    run.output,
                    from,
                ));
            }
        };
        let mut manifest = self.read_manifest();
        manifest.insert(
            pin.name.clone(),
            ManifestEntry {
                package: pin.package.clone(),
                version: to.clone(),
                installed_at: Utc::now(),
            },
        );
        let mut output = run.output;
        if let Err(e) = self.write_manifest(&manifest) {
            output.push_str(&format!("\ncannot write the manifest: {e}\n"));
        }
        if pin.harness == HarnessId::Copilot {
            match self.ensure_extension(true).await {
                Ok(s) => output.push_str(&format!("\nCopilot prompt extension: {s}\n")),
                Err(e) => output.push_str(&format!("\nCopilot prompt extension: failed: {e}\n")),
            }
        }
        Ok(Updated {
            name: pin.name.clone(),
            package: pin.package.clone(),
            from,
            to,
            output,
        })
    }

    /// The marker text: the extension and Copilot versions in the tools dir.
    fn extension_marker(&self) -> Option<String> {
        let ext = self.pins.iter().find(|p| p.name == COPILOT_PROMPT)?;
        let copilot = self
            .pins
            .iter()
            .find(|p| !p.extension && p.harness == HarnessId::Copilot);
        let copilot_v = copilot
            .and_then(|p| self.present(p))
            .unwrap_or_else(|| "none".into());
        Some(format!(
            "copilot-prompt-{} copilot-{copilot_v}",
            self.present(ext)?
        ))
    }

    /// Install the Copilot prompt extension into the Copilot home
    /// (`copilot-custom-system-prompt install --force`) when its marker
    /// differs, or always with `force`. Answers what it did.
    pub async fn ensure_extension(&self, force: bool) -> Result<String, String> {
        let Some(ext) = self.pins.iter().find(|p| p.name == COPILOT_PROMPT) else {
            return Ok("no extension pin".into());
        };
        let Some(marker) = self.extension_marker() else {
            return Ok(format!("skipped: {} is not installed", ext.package));
        };
        if !self.copilot_home.is_dir() {
            return Ok(format!(
                "skipped: the Copilot home {} does not exist",
                self.copilot_home.display()
            ));
        }
        let marker_path = self.copilot_home.join(EXTENSION_MARKER);
        let old = std::fs::read_to_string(&marker_path).unwrap_or_default();
        if !force && old.trim() == marker {
            return Ok("present".into());
        }
        let bin = self.bin_dir().join(&ext.binary);
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new(&bin)
                .args(["install", "--force"])
                .env("COPILOT_HOME", &self.copilot_home)
                .env("PATH", self.path_env())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| format!("{} install did not finish in 60 s", bin.display()))?
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
        if !out.status.success() {
            return Err(format!(
                "{} install failed: {}",
                bin.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        std::fs::write(&marker_path, format!("{marker}\n"))
            .map_err(|e| format!("cannot write {}: {e}", marker_path.display()))?;
        Ok(format!(
            "installed into {} ({marker})",
            self.copilot_home.display()
        ))
    }
}

/// `dir` first on `path`, with any other copy of `dir` removed.
pub fn prepend_path(dir: &Path, path: &str) -> String {
    let d = dir.display().to_string();
    let rest: Vec<&str> = path
        .split(':')
        .filter(|p| !p.is_empty() && *p != d)
        .collect();
    if rest.is_empty() {
        d
    } else {
        format!("{d}:{}", rest.join(":"))
    }
}

/// The tools dir of a running daemon, and the lock that lets one update
/// run at a time (`POST /api/harnesses/{name}/update`).
#[derive(Debug, Default)]
pub struct HarnessTools {
    cfg: RwLock<Option<Arc<ToolsConfig>>>,
    /// Held while an update runs.
    pub busy: tokio::sync::Mutex<()>,
}

impl HarnessTools {
    /// Set the tools dir. Without one, the update endpoint answers
    /// `501 not_available`.
    pub fn configure(&self, cfg: ToolsConfig) {
        *self
            .cfg
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(cfg));
    }

    pub fn config(&self) -> Option<Arc<ToolsConfig>> {
        self.cfg
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A fake npm for tests (here and in `tests/harness_tools.rs`). It appends
/// its argv to `npm.log` next to itself. For each `<package>@<version>` it
/// writes `<prefix>/lib/node_modules/<package>/package.json` and a
/// `<prefix>/bin/<binary>` that answers `--version` as Copilot does,
/// with a full stop after the version (and `install`, as the
/// Copilot prompt extension does, into `$COPILOT_HOME`). `latest` installs
/// 9.0.0. A version that starts with `99.` fails like npm's ETARGET.
#[doc(hidden)]
pub const FAKE_NPM: &str = r#"#!/bin/sh
log="$(dirname "$0")/npm.log"
echo "$*" >> "$log"
prefix=""
next=0
for a in "$@"; do
  if [ "$next" = 1 ]; then prefix="$a"; next=0; continue; fi
  case "$a" in --prefix) next=1 ;; esac
done
for a in "$@"; do
  case "$a" in
    -*|install|"$prefix") continue ;;
  esac
  pkg="${a%@*}"; ver="${a##*@}"
  case "$ver" in 99.*)
    echo "npm error code ETARGET" >&2
    echo "npm error notarget No matching version found for $a." >&2
    exit 1 ;;
  esac
  [ "$ver" = latest ] && ver="9.0.0"
  case "$pkg" in
    @anthropic-ai/claude-code) bin=claude ;;
    @openai/codex) bin=codex ;;
    @github/copilot) bin=copilot ;;
    @earendil-works/pi-coding-agent) bin=pi ;;
    copilot-custom-system-prompt) bin=copilot-custom-system-prompt ;;
    *) echo "npm error 404 Not Found - $pkg" >&2; exit 1 ;;
  esac
  mkdir -p "$prefix/lib/node_modules/$pkg" "$prefix/bin"
  printf '{"name":"%s","version":"%s"}\n' "$pkg" "$ver" > "$prefix/lib/node_modules/$pkg/package.json"
  cat > "$prefix/bin/$bin" <<EOF
#!/bin/sh
if [ "\$1" = install ]; then mkdir -p "\$COPILOT_HOME/extensions/x" && echo "$ver" > "\$COPILOT_HOME/extensions/x/v"; echo installed; exit 0; fi
echo "$bin CLI $ver."
EOF
  chmod +x "$prefix/bin/$bin"
  echo "added 1 package: $pkg@$ver"
done
"#;

/// Write [`FAKE_NPM`] as `<dir>/npm`, executable, and answer its path.
#[doc(hidden)]
pub fn write_fake_npm(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let npm = dir.join("npm");
    let _ = std::fs::write(&npm, FAKE_NPM);
    let _ = std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755));
    npm
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    pub(crate) fn fake_npm(dir: &Path) -> PathBuf {
        write_fake_npm(dir)
    }

    pub(crate) fn config(tmp: &Path) -> ToolsConfig {
        let home = tmp.join("copilot-home");
        std::fs::create_dir_all(&home).unwrap();
        let mut c = ToolsConfig::new(tmp.join("tools"), compiled_pins(), home);
        c.npm = fake_npm(tmp);
        c
    }

    fn npm_log(tmp: &Path) -> Vec<String> {
        std::fs::read_to_string(tmp.join("npm.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_pins_file_and_the_compiled_pins_agree() {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../harnesses.toml"))
                .unwrap();
        let mut a = parse_pins(&text).unwrap();
        let mut b = compiled_pins();
        a.sort_by(|x, y| x.name.cmp(&y.name));
        b.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(a, b);
        assert!(b.iter().any(|p| p.name == COPILOT_PROMPT && p.extension));
        let (p, src) = load_pins(Path::new("/nonexistent/harnesses.toml"));
        assert_eq!((p.len(), src.as_str()), (b.len(), "built in"));
    }

    #[test]
    fn versions_and_path() {
        assert!(
            valid_version("latest") && valid_version("0.164.0") && valid_version("1.5.0-beta.1")
        );
        for v in ["", "1.2", "--global", "1.2.3; rm -rf /", "next", "^1.2.3"] {
            assert!(!valid_version(v), "{v}");
        }
        assert_eq!(
            prepend_path(Path::new("/t/bin"), "/usr/bin:/t/bin:/bin"),
            "/t/bin:/usr/bin:/bin"
        );
        assert_eq!(prepend_path(Path::new("/t/bin"), ""), "/t/bin");
    }

    /// The first install runs npm once for each pin, with the pinned
    /// version and the tools dir as the prefix, checks `--version`, writes
    /// the manifest, and installs the extension. The second one is a no-op.
    #[tokio::test]
    async fn install_fills_an_empty_dir_then_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        let r = c.install().await;
        assert_eq!(r.failed(), 0, "{r:#?}");
        assert!(r.items.iter().all(|i| i.action == "installed"), "{r:#?}");
        let log = npm_log(tmp.path());
        assert_eq!(log.len(), c.pins.len(), "{log:?}");
        let dir = c.dir.display().to_string();
        for p in &c.pins {
            let want = format!(
                "install -g --no-audit --no-fund --prefix {dir} {}@{}",
                p.package, p.version
            );
            assert!(log.contains(&want), "{want} not in {log:?}");
        }
        let m = c.read_manifest();
        assert_eq!(m.len(), c.pins.len());
        let codex = c.pin("codex").unwrap();
        assert_eq!(m["codex"].version, codex.version);
        assert_eq!(m["codex"].package, "@openai/codex");
        assert!(r.extension.starts_with("installed into"), "{}", r.extension);
        let marker = std::fs::read_to_string(c.copilot_home.join(EXTENSION_MARKER)).unwrap();
        assert!(marker.starts_with("copilot-prompt-"), "{marker}");
        let stamp = m["codex"].installed_at;

        let again = c.install().await;
        assert!(
            again.items.iter().all(|i| i.action == "present"),
            "{again:#?}"
        );
        assert_eq!(again.extension, "present");
        assert_eq!(
            npm_log(tmp.path()).len(),
            c.pins.len(),
            "no npm run the second time"
        );
        assert_eq!(c.read_manifest()["codex"].installed_at, stamp);
    }

    /// A present package keeps its version: an update survives a restart.
    /// A missing one comes back at the pin.
    #[tokio::test]
    async fn install_keeps_an_updated_package_and_restores_a_missing_one() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        c.install().await;
        let u = c.update("codex", "latest").await.unwrap();
        assert_eq!(u.to, "9.0.0");
        std::fs::remove_file(c.bin_dir().join("pi")).unwrap();
        let r = c.install().await;
        let item = |n: &str| r.items.iter().find(|i| i.name == n).unwrap().clone();
        assert_eq!(item("codex").action, "present");
        assert_eq!(item("codex").version.as_deref(), Some("9.0.0"));
        assert_eq!(item("pi").action, "installed");
        assert_eq!(c.read_manifest()["codex"].version, "9.0.0");
    }

    #[tokio::test]
    async fn update_runs_npm_in_place_and_reports_from_and_to() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        c.install().await;
        let pin = c.pin("codex").unwrap().version.clone();
        let same = c.update("codex", &pin).await.unwrap();
        assert_eq!(
            (same.from.as_deref(), same.to.as_str()),
            (Some(pin.as_str()), pin.as_str())
        );
        assert!(same.output.contains("added 1 package"), "{}", same.output);
        let up = c.update("codex", "0.170.1").await.unwrap();
        assert_eq!(
            (up.from.as_deref(), up.to.as_str()),
            (Some(pin.as_str()), "0.170.1")
        );
        assert_eq!(c.read_manifest()["codex"].version, "0.170.1");
        assert!(
            npm_log(tmp.path())
                .last()
                .unwrap()
                .ends_with("@openai/codex@0.170.1")
        );

        // A Copilot update reinstalls the extension.
        let cp = c.update("copilot", "1.0.200").await.unwrap();
        assert!(
            cp.output.contains("Copilot prompt extension: installed"),
            "{}",
            cp.output
        );
        let marker = std::fs::read_to_string(c.copilot_home.join(EXTENSION_MARKER)).unwrap();
        assert!(marker.contains("copilot-1.0.200"), "{marker}");
    }

    #[tokio::test]
    async fn a_failed_npm_run_gives_its_output_and_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        c.install().await;
        let before = c.read_manifest();
        let e = c.update("codex", "99.0.0").await.unwrap_err();
        assert!(e.message.contains("failed (exit 1)"), "{}", e.message);
        assert!(e.output.contains("ETARGET"), "{}", e.output);
        assert_eq!(
            e.from.as_deref(),
            Some(c.pin("codex").unwrap().version.as_str())
        );
        assert_eq!(c.read_manifest(), before);

        let runs = npm_log(tmp.path()).len();
        let e = c.update("codex", "--global").await.unwrap_err();
        assert!(e.message.contains("not `latest`"), "{}", e.message);
        assert!(c.update("nope", "latest").await.is_err());
        assert_eq!(
            npm_log(tmp.path()).len(),
            runs,
            "a bad version never reaches npm"
        );
    }

    #[tokio::test]
    async fn a_missing_npm_fails_each_pin_with_the_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = config(tmp.path());
        c.npm = tmp.path().join("no-npm");
        let r = c.install().await;
        assert_eq!(r.failed(), c.pins.len());
        assert!(
            r.items[0].error.as_deref().unwrap().contains("cannot run"),
            "{r:#?}"
        );
        assert!(r.extension.starts_with("skipped"), "{}", r.extension);
    }
}
