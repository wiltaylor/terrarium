//! The harness tools dir: the harnesses live on a persistent volume, not in
//! the image (docs/getting-started.md §7, docs/CONVENTIONS.md).
//!
//! The tools dir (`TERRARIUM_TOOLS_DIR`, `/opt/terrarium/tools` in the
//! image, `./tools` outside Docker) holds one npm prefix for each version
//! of each harness. The sandbox binds the dir read-only
//! (`crate::sandbox`):
//!
//! ```text
//! tools/
//!   versions/<name>/<version>/   npm prefix: lib/node_modules/<package>, bin/<binary>, .complete
//!   current/<name>               -> ../versions/<name>/<version>   (the active version)
//!   bin/<binary>                 -> ../versions/<name>/<version>/bin/<binary>   (first on PATH)
//!   terrarium-manifest.json
//! ```
//!
//! A version dir does not change after it is complete: npm installs into
//! `<version>.tmp-<id>`, the daemon checks `--version`, writes the marker
//! `.complete`, and renames the dir into place. A switch replaces the two
//! symlinks with `rename`, so it is atomic. An agent launch resolves the
//! active version dir and runs the harness from it
//! ([`active_version`], `crate::runner_supervisor`), so a later switch
//! does not change a running agent. The next launch (a restart, or a new
//! agent) takes the active version.
//!
//! The operations:
//!
//! - [`ToolsConfig::install`] (`terrariumd harness install`, run by the
//!   entrypoint and by the daemon at each start): move the flat layout of
//!   before the version dirs into version dirs ([`ToolsConfig::migrate_flat`]),
//!   then install each pin of `harnesses.toml` that has no active version.
//!   An active version stays, so an update survives a restart. Then install
//!   the Copilot prompt extension into the Copilot home when its marker
//!   changed.
//! - [`ToolsConfig::update`] (`POST /api/harnesses/{name}/update`): install
//!   a version dir (no npm run when the version is complete on disk), then
//!   switch to it.
//! - [`ToolsConfig::gc`]: remove the version dirs that are not active and
//!   that no live agent uses.
//!
//! Each writes the manifest `<dir>/terrarium-manifest.json`:
//! `{"<name>": {"package", "version", "installed_at", "versions"}}`, where
//! `version` is the active version and `versions` the complete version
//! dirs on disk.

use std::collections::{BTreeMap, BTreeSet};
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
/// The version dirs: `<dir>/versions/<name>/<version>/`.
pub const VERSIONS_DIR: &str = "versions";
/// The active version of each pin: `<dir>/current/<name>`, a symlink to
/// `../versions/<name>/<version>`.
pub const CURRENT_DIR: &str = "current";
/// The marker of a complete version dir. It holds the package, the
/// version, and the install time (JSON).
pub const COMPLETE: &str = ".complete";
/// The most version dirs of one pin that [`ToolsConfig::gc`] keeps when it
/// does not know which versions the live agents use (`terrariumd harness
/// install`).
pub const KEEP_VERSIONS: usize = 3;
/// The mark in the name of a version dir that npm still fills:
/// `<version>.tmp-<id>`.
const TMP_MARK: &str = ".tmp-";
/// The mark in the name of a version dir that a reinstall replaced:
/// `<version>.old-<id>`. The next GC removes it.
const OLD_MARK: &str = ".old-";
/// A temp dir older than this is a leftover of a stopped install.
const STALE_TMP: Duration = Duration::from_secs(30 * 60);
/// The flat layout of before the version dirs: npm's own prefix dirs.
const FLAT_MODULES: &str = "lib/node_modules";

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
    /// The active version.
    pub version: String,
    /// When the active version dir was installed.
    pub installed_at: DateTime<Utc>,
    /// The complete version dirs on disk, oldest first.
    #[serde(default)]
    pub versions: Vec<String>,
}

/// The manifest: one row for each installed package, by pin name.
pub type Manifest = BTreeMap<String, ManifestEntry>;

/// The versions that the live agents run, by pin name
/// ([`ToolsConfig::gc`]).
pub type InUse = BTreeMap<String, BTreeSet<String>>;

/// True for a version the update accepts: `latest`, or a semver version.
/// Nothing else reaches the npm command line.
pub fn valid_version(v: &str) -> bool {
    v == "latest" || semver::Version::parse(v).is_ok()
}

/// The active version of the pin `name` in the tools dir `tools`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveVersion {
    pub version: String,
    /// `<tools>/versions/<name>/<version>`: the npm prefix of the version.
    pub dir: PathBuf,
}

/// The active version of the pin `name`: the target of
/// `<tools>/current/<name>`, when it names a complete version dir. The
/// answer is the path under `tools`, not the real path, so it stays under
/// the dir that the sandbox binds.
pub fn active_version(tools: &Path, name: &str) -> Option<ActiveVersion> {
    let target = std::fs::read_link(tools.join(CURRENT_DIR).join(name)).ok()?;
    let version = target.file_name()?.to_str()?.to_owned();
    if semver::Version::parse(&version).is_err() {
        return None;
    }
    let dir = tools.join(VERSIONS_DIR).join(name).join(&version);
    dir.join(COMPLETE)
        .is_file()
        .then_some(ActiveVersion { version, dir })
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
    /// The active version after the step; `None` on a failure.
    pub version: Option<String>,
    /// `present` (no change), `migrated` (moved from the flat layout),
    /// `installed`, or `failed`.
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
    /// The version dirs that a GC after the install removed
    /// (`<name> <version>`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
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
    /// True when the version dir was complete on disk, so npm did not run.
    pub reused: bool,
}

/// A failed update: the message and npm's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFailure {
    pub message: String,
    pub output: String,
    pub from: Option<String>,
}

/// A complete version dir, from [`ToolsConfig::install_version`].
struct Installed {
    version: String,
    output: String,
    reused: bool,
}

/// The content of [`COMPLETE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Complete {
    package: String,
    version: String,
    installed_at: DateTime<Utc>,
}

/// A short unique suffix for a temp name.
fn nonce() -> String {
    ulid::Ulid::new().to_string().to_lowercase()
}

/// The order of version dirs: by semver, then by name.
fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

/// Make `link` a symlink to `target`, atomically: a new link next to it,
/// then `rename` over the old one.
fn replace_symlink(link: &Path, target: &Path) -> std::io::Result<()> {
    let parent = link
        .parent()
        .ok_or_else(|| std::io::Error::other(format!("{} has no parent", link.display())))?;
    std::fs::create_dir_all(parent)?;
    let name = link.file_name().and_then(|n| n.to_str()).unwrap_or("link");
    let tmp = parent.join(format!(".{name}{TMP_MARK}{}", nonce()));
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, link).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Copy the tree `src` to `dst` with hard links: no file data moves, and
/// the files of `src` stay as they are. A symlink stays a symlink with the
/// same target. A file that cannot be linked (another file system) is
/// copied.
fn link_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = std::fs::symlink_metadata(&from)?.file_type();
        if kind.is_dir() {
            link_tree(&from, &to)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if std::fs::hard_link(&from, &to).is_err() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Remove the version dir `dir`: first its marker, so a half-removed dir
/// never counts as complete.
fn remove_version_dir(dir: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(dir.join(COMPLETE));
    std::fs::remove_dir_all(dir)
}

/// The version in `<prefix>/lib/node_modules/<package>/package.json`.
fn package_version_at(prefix: &Path, pin: &Pin) -> Option<String> {
    let path = prefix
        .join(FLAT_MODULES)
        .join(&pin.package)
        .join("package.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v["version"].as_str().map(str::to_owned)
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

    /// `<dir>/versions/<name>`.
    pub fn versions_dir(&self, name: &str) -> PathBuf {
        self.dir.join(VERSIONS_DIR).join(name)
    }

    /// `<dir>/versions/<name>/<version>`.
    pub fn version_dir(&self, name: &str, version: &str) -> PathBuf {
        self.versions_dir(name).join(version)
    }

    /// The active version of `pin`.
    pub fn active(&self, pin: &Pin) -> Option<ActiveVersion> {
        active_version(&self.dir, &pin.name)
    }

    /// The complete version dirs of the pin `name`, oldest first.
    pub fn versions(&self, name: &str) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.versions_dir(name)) else {
            return Vec::new();
        };
        let mut out: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| semver::Version::parse(n).is_ok())
            .filter(|n| self.version_dir(name, n).join(COMPLETE).is_file())
            .collect();
        out.sort_by(|a, b| version_order(a, b));
        out
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

    /// Write the manifest from the version dirs on disk. A pin with no
    /// active version has no row.
    pub fn sync_manifest(&self) -> std::io::Result<()> {
        let old = self.read_manifest();
        let mut m = Manifest::new();
        for pin in &self.pins {
            let Some(a) = self.active(pin) else { continue };
            let installed_at = std::fs::read_to_string(a.dir.join(COMPLETE))
                .ok()
                .and_then(|t| serde_json::from_str::<Complete>(&t).ok())
                .map(|c| c.installed_at)
                .unwrap_or_else(Utc::now);
            m.insert(
                pin.name.clone(),
                ManifestEntry {
                    package: pin.package.clone(),
                    version: a.version,
                    installed_at,
                    versions: self.versions(&pin.name),
                },
            );
        }
        if m == old {
            return Ok(());
        }
        self.write_manifest(&m)
    }

    /// The version of the active version dir of `pin`.
    pub fn package_version(&self, pin: &Pin) -> Option<String> {
        package_version_at(&self.active(pin)?.dir, pin)
    }

    /// The active version, when its package and its command are there.
    pub fn present(&self, pin: &Pin) -> Option<String> {
        let a = self.active(pin)?;
        if !a.dir.join("bin").join(&pin.binary).exists() {
            return None;
        }
        package_version_at(&a.dir, pin)
    }

    /// `<prefix>/bin/<binary> --version` for a harness; the package version
    /// for an extension (its command has no `--version`).
    async fn verify_at(&self, pin: &Pin, prefix: &Path) -> Result<String, String> {
        if pin.extension {
            return package_version_at(prefix, pin)
                .ok_or_else(|| format!("{} is not in {}", pin.package, prefix.display()));
        }
        let bin = prefix.join("bin").join(&pin.binary);
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new(&bin)
                .arg("--version")
                .env("PATH", self.path_env(Some(&prefix.join("bin"))))
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

    /// `PATH` with `first` (when given), then the tools `bin/`, first.
    fn path_env(&self, first: Option<&Path>) -> String {
        let rest = std::env::var("PATH").unwrap_or_default();
        let path = prepend_path(&self.bin_dir(), &rest);
        match first {
            Some(f) => prepend_path(f, &path),
            None => path,
        }
    }

    /// `npm install -g <specs>` into the prefix `prefix`.
    pub async fn npm_install(&self, prefix: &Path, specs: &[String]) -> NpmRun {
        let mut cmd = tokio::process::Command::new(&self.npm);
        cmd.args(["install", "-g", "--no-audit", "--no-fund", "--prefix"])
            .arg(prefix)
            .args(specs)
            .env("NPM_CONFIG_PREFIX", prefix)
            .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
            .env("PATH", self.path_env(None))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // npm runs package scripts: no secret of the daemon goes to them.
        for k in crate::password::SECRET_ENV
            .into_iter()
            .chain(["TERRARIUM_TOKEN"])
        {
            cmd.env_remove(k);
        }
        let child = match spawn_retrying(&mut cmd).await {
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

    /// Make the version dir of `<package>@<wanted>` complete. A complete
    /// dir of a concrete version stays as it is, with no npm run, unless
    /// `force`. Else npm installs into `<version>.tmp-<id>`, the command
    /// answers `--version`, the marker goes in, and the dir is renamed into
    /// place. A failure removes the temp dir; the other version dirs and
    /// the symlinks do not change. The error is the message and npm's
    /// output.
    async fn install_version(
        &self,
        pin: &Pin,
        wanted: &str,
        force: bool,
    ) -> Result<Installed, (String, String)> {
        let base = self.versions_dir(&pin.name);
        if wanted != "latest"
            && !force
            && self.version_dir(&pin.name, wanted).join(COMPLETE).is_file()
        {
            return Ok(Installed {
                version: wanted.to_owned(),
                output: format!(
                    "{}@{wanted} is complete in {}: no npm run\n",
                    pin.package,
                    base.display()
                ),
                reused: true,
            });
        }
        let tmp = base.join(format!("{wanted}{TMP_MARK}{}", nonce()));
        if let Err(e) = std::fs::create_dir_all(&tmp) {
            return Err((format!("cannot make {}: {e}", tmp.display()), String::new()));
        }
        let spec = format!("{}@{wanted}", pin.package);
        tracing::info!(package = %spec, prefix = %tmp.display(), "installing a harness version");
        let run = self.npm_install(&tmp, std::slice::from_ref(&spec)).await;
        let result = self.finish_version(pin, &spec, &tmp, &run).await;
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        result
            .map(|version| Installed {
                version,
                output: run.output.clone(),
                reused: false,
            })
            .map_err(|e| (e, run.output))
    }

    /// Check the npm run into `tmp`, write the marker, and rename the dir
    /// to `<version>`. Answers the version.
    async fn finish_version(
        &self,
        pin: &Pin,
        spec: &str,
        tmp: &Path,
        run: &NpmRun,
    ) -> Result<String, String> {
        if !run.ok() {
            return Err(run.failure(spec));
        }
        self.verify_at(pin, tmp)
            .await
            .map_err(|e| format!("npm installed {spec}, but {e}"))?;
        let version = package_version_at(tmp, pin)
            .filter(|v| semver::Version::parse(v).is_ok())
            .ok_or_else(|| {
                format!("npm installed {spec}, but its package.json has no version like 1.2.3")
            })?;
        let marker = serde_json::to_string(&Complete {
            package: pin.package.clone(),
            version: version.clone(),
            installed_at: Utc::now(),
        })
        .map_err(|e| e.to_string())?;
        std::fs::write(tmp.join(COMPLETE), marker + "\n")
            .map_err(|e| format!("cannot write the marker in {}: {e}", tmp.display()))?;
        self.put_in_place(&pin.name, &version, tmp)
            .map_err(|e| format!("cannot move {} into place: {e}", tmp.display()))?;
        Ok(version)
    }

    /// Rename the complete dir `tmp` to `versions/<name>/<version>`. A dir
    /// that is there already (a reinstall) goes aside first as
    /// `<version>.old-<id>`; the next GC removes it.
    fn put_in_place(&self, name: &str, version: &str, tmp: &Path) -> std::io::Result<()> {
        let dest = self.version_dir(name, version);
        if std::fs::symlink_metadata(&dest).is_ok() {
            let aside = self
                .versions_dir(name)
                .join(format!("{version}{OLD_MARK}{}", nonce()));
            std::fs::rename(&dest, aside)?;
        }
        std::fs::rename(tmp, dest)
    }

    /// Switch `pin` to its complete version dir `version`: replace
    /// `bin/<binary>` and `current/<name>` (each with an atomic rename).
    pub fn activate(&self, pin: &Pin, version: &str) -> std::io::Result<()> {
        if !self
            .version_dir(&pin.name, version)
            .join(COMPLETE)
            .is_file()
        {
            return Err(std::io::Error::other(format!(
                "{} {version} is not a complete version dir",
                pin.name
            )));
        }
        let rel = Path::new("..")
            .join(VERSIONS_DIR)
            .join(&pin.name)
            .join(version);
        replace_symlink(
            &self.bin_dir().join(&pin.binary),
            &rel.join("bin").join(&pin.binary),
        )?;
        replace_symlink(&self.dir.join(CURRENT_DIR).join(&pin.name), &rel)
    }

    /// True when the flat layout of before the version dirs is there
    /// (`<dir>/lib/node_modules`).
    pub fn has_flat_layout(&self) -> bool {
        self.dir.join(FLAT_MODULES).is_dir()
    }

    /// Move each package of the flat layout that has no active version into
    /// a version dir, and switch to it. The flat files stay as they are:
    /// the version dir gets hard links to them, so an agent that started on
    /// the flat layout keeps its files. [`Self::remove_flat`] removes them
    /// later. Answers the pins that moved.
    pub fn migrate_flat(&self) -> Vec<String> {
        let mut moved = Vec::new();
        if !self.has_flat_layout() {
            return moved;
        }
        for pin in &self.pins {
            if self.active(pin).is_some() {
                continue;
            }
            let Some(version) =
                package_version_at(&self.dir, pin).filter(|v| semver::Version::parse(v).is_ok())
            else {
                continue;
            };
            match self.migrate_one(pin, &version) {
                Ok(true) => {
                    tracing::info!(harness = %pin.name, version, "moved from the flat layout into a version dir");
                    moved.push(pin.name.clone());
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(harness = %pin.name, version, "cannot move the flat layout into a version dir: {e}");
                }
            }
        }
        moved
    }

    /// [`Self::migrate_flat`] for one pin. `Ok(false)`: its command in the
    /// flat `bin/` is not a link into its package (the install gets it
    /// from npm instead).
    fn migrate_one(&self, pin: &Pin, version: &str) -> std::io::Result<bool> {
        let rel_pkg = Path::new("..").join(FLAT_MODULES).join(&pin.package);
        let bin = self.bin_dir().join(&pin.binary);
        let flat_bin = std::fs::symlink_metadata(&bin)?;
        let link = if flat_bin.file_type().is_symlink() {
            let t = std::fs::read_link(&bin)?;
            if !t.starts_with(&rel_pkg) {
                return Ok(false);
            }
            Some(t)
        } else {
            None
        };
        if !self
            .version_dir(&pin.name, version)
            .join(COMPLETE)
            .is_file()
        {
            let tmp = self
                .versions_dir(&pin.name)
                .join(format!("{version}{TMP_MARK}{}", nonce()));
            let r = (|| {
                link_tree(
                    &self.dir.join(FLAT_MODULES).join(&pin.package),
                    &tmp.join(FLAT_MODULES).join(&pin.package),
                )?;
                std::fs::create_dir_all(tmp.join("bin"))?;
                let to = tmp.join("bin").join(&pin.binary);
                match &link {
                    Some(t) => std::os::unix::fs::symlink(t, &to)?,
                    None => {
                        if std::fs::hard_link(&bin, &to).is_err() {
                            std::fs::copy(&bin, &to)?;
                        }
                    }
                }
                let marker = serde_json::to_string(&Complete {
                    package: pin.package.clone(),
                    version: version.to_owned(),
                    installed_at: Utc::now(),
                })
                .map_err(std::io::Error::other)?;
                std::fs::write(tmp.join(COMPLETE), marker + "\n")?;
                self.put_in_place(&pin.name, version, &tmp)
            })();
            if let Err(e) = r {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
        }
        self.activate(pin, version)?;
        Ok(true)
    }

    /// Remove the flat layout (`<dir>/lib`), and each command in `bin/`
    /// that still points into it. Call it only when no live agent started
    /// on the flat layout. Answers true when something was removed.
    pub fn remove_flat(&self) -> std::io::Result<bool> {
        if !self.has_flat_layout() {
            return Ok(false);
        }
        if let Ok(rd) = std::fs::read_dir(self.bin_dir()) {
            for e in rd.flatten() {
                if std::fs::read_link(e.path()).is_ok_and(|t| t.starts_with("../lib")) {
                    std::fs::remove_file(e.path())?;
                }
            }
        }
        std::fs::remove_dir_all(self.dir.join("lib"))?;
        tracing::info!(dir = %self.dir.display(), "removed the flat harness layout");
        Ok(true)
    }

    /// Remove the version dirs that are not needed. Each pin keeps its
    /// active version. With `in_use` (the daemon knows its live agents),
    /// it also keeps each version that a live agent runs and removes the
    /// rest. Without it, it keeps the newest complete versions, up to
    /// [`KEEP_VERSIONS`] in all. A temp dir of a stopped install goes after
    /// [`STALE_TMP`]; a dir that a reinstall replaced goes at once.
    /// Answers the removed versions (`<name> <version>`).
    pub fn gc(&self, in_use: Option<&InUse>) -> Vec<String> {
        let mut removed = Vec::new();
        for pin in &self.pins {
            let base = self.versions_dir(&pin.name);
            let Ok(rd) = std::fs::read_dir(&base) else {
                continue;
            };
            let active = self.active(pin).map(|a| a.version);
            let mut complete = Vec::new();
            for e in rd.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let path = e.path();
                let stale = |p: &Path| {
                    std::fs::symlink_metadata(p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > STALE_TMP)
                };
                let drop = if name.contains(TMP_MARK) {
                    stale(&path)
                } else if name.contains(OLD_MARK) {
                    true
                } else if path.join(COMPLETE).is_file() && semver::Version::parse(&name).is_ok() {
                    complete.push(name.clone());
                    false
                } else {
                    // Not complete, and not an install that runs.
                    active.as_deref() != Some(name.as_str())
                };
                if drop {
                    match remove_version_dir(&path) {
                        Ok(()) => removed.push(format!("{} {name}", pin.name)),
                        Err(e) => tracing::warn!(dir = %path.display(), "cannot remove: {e}"),
                    }
                }
            }
            complete.sort_by(|a, b| version_order(b, a));
            let mut keep: BTreeSet<String> = active.into_iter().collect();
            match in_use {
                Some(u) => keep.extend(u.get(&pin.name).into_iter().flatten().cloned()),
                None => {
                    for v in &complete {
                        if keep.len() >= KEEP_VERSIONS {
                            break;
                        }
                        keep.insert(v.clone());
                    }
                }
            }
            for v in complete.iter().rev().filter(|v| !keep.contains(*v)) {
                let path = base.join(v);
                match remove_version_dir(&path) {
                    Ok(()) => {
                        tracing::info!(harness = %pin.name, version = %v, "removed an unused harness version");
                        removed.push(format!("{} {v}", pin.name));
                    }
                    Err(e) => tracing::warn!(dir = %path.display(), "cannot remove: {e}"),
                }
            }
        }
        if let Err(e) = self.sync_manifest() {
            tracing::warn!("cannot write {}: {e}", self.manifest_path().display());
        }
        removed
    }

    /// Move the flat layout into version dirs, then install each pin that
    /// has no active version; keep each active version as it is. Then the
    /// Copilot prompt extension. Writes the manifest. No version dir is
    /// removed: the caller runs [`Self::gc`].
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
                removed: Vec::new(),
            };
        }
        let moved = self.migrate_flat();
        for pin in &self.pins {
            if let Some(v) = self.present(pin) {
                // `bin/` must point at the active version (a manual
                // `npm install -g` into the tools dir replaces the link).
                if let Some(a) = self.active(pin) {
                    let want = Path::new("..")
                        .join(VERSIONS_DIR)
                        .join(&pin.name)
                        .join(&a.version)
                        .join("bin")
                        .join(&pin.binary);
                    let bin = self.bin_dir().join(&pin.binary);
                    if std::fs::read_link(&bin).ok().as_deref() != Some(want.as_path()) {
                        tracing::warn!(harness = %pin.name, version = %a.version, "bin/{} did not point at the active version; switching it back", pin.binary);
                        if let Err(e) = self.activate(pin, &a.version) {
                            tracing::warn!(harness = %pin.name, "cannot switch bin/{}: {e}", pin.binary);
                        }
                    }
                }
                let action = if moved.contains(&pin.name) {
                    "migrated"
                } else {
                    "present"
                };
                items.push(InstallItem {
                    name: pin.name.clone(),
                    package: pin.package.clone(),
                    version: Some(v),
                    action: action.into(),
                    error: None,
                    output: String::new(),
                });
                continue;
            }
            let installed = match self.install_version(pin, &pin.version, false).await {
                Ok(i) => self.activate(pin, &i.version).map(|()| i).map_err(|e| {
                    (
                        format!("cannot switch to {}: {e}", pin.version),
                        String::new(),
                    )
                }),
                Err(e) => Err(e),
            };
            match installed {
                Ok(i) => items.push(InstallItem {
                    name: pin.name.clone(),
                    package: pin.package.clone(),
                    version: Some(i.version),
                    action: "installed".into(),
                    error: None,
                    output: i.output,
                }),
                Err((e, output)) => items.push(InstallItem {
                    name: pin.name.clone(),
                    package: pin.package.clone(),
                    version: None,
                    action: "failed".into(),
                    error: Some(e),
                    output,
                }),
            }
        }
        if let Err(e) = self.sync_manifest() {
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
            removed: Vec::new(),
        }
    }

    /// Install `<package>@<version>` of the pin `name` into its version dir
    /// (no npm run when that dir is complete, unless `force`), check it,
    /// and switch to it. A running agent keeps its version dir; the caller
    /// runs [`Self::gc`]. A Copilot update also reinstalls the prompt
    /// extension.
    pub async fn update(
        &self,
        name: &str,
        version: &str,
        force: bool,
    ) -> Result<Updated, UpdateFailure> {
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
        tracing::info!(package = %pin.package, version, from = from.as_deref().unwrap_or("-"), "updating a harness");
        let installed = self
            .install_version(pin, version, force)
            .await
            .map_err(|(m, o)| fail(m, o, from.clone()))?;
        if let Err(e) = self.activate(pin, &installed.version) {
            return Err(fail(
                format!("cannot switch {} to {}: {e}", pin.name, installed.version),
                installed.output,
                from,
            ));
        }
        let mut output = installed.output;
        if let Err(e) = self.sync_manifest() {
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
            to: installed.version,
            output,
            reused: installed.reused,
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
    /// differs, or always with `force`. Answers what it did. The extension
    /// is a copy in the Copilot home, not a link into the tools dir, so a
    /// switch of a version dir does not change it.
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
                .env("PATH", self.path_env(None))
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
/// Spawn `cmd`, again up to 4 times on `ETXTBSY`. A program file that was
/// just written (an npm update, or a test's fake npm) can still be open for
/// writing in a child that another thread forked and that has not run
/// `exec` yet; the error goes away within milliseconds.
async fn spawn_retrying(
    cmd: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    const ETXTBSY: i32 = 26;
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && tries < 4 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(25 * tries)).await;
            }
            r => return r,
        }
    }
}

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

    /// The target of a symlink, as text.
    fn link(p: &Path) -> String {
        std::fs::read_link(p).unwrap().display().to_string()
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
    /// version and a temp dir under `versions/<name>/` as the prefix,
    /// renames the dir into place with its marker, points `bin/` and
    /// `current/` at it, writes the manifest, and installs the extension.
    /// The second one is a no-op.
    #[tokio::test]
    async fn install_fills_an_empty_dir_then_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        let r = c.install().await;
        assert_eq!(r.failed(), 0, "{r:#?}");
        assert!(r.items.iter().all(|i| i.action == "installed"), "{r:#?}");
        let log = npm_log(tmp.path());
        assert_eq!(log.len(), c.pins.len(), "{log:?}");
        for p in &c.pins {
            let prefix = format!(
                "install -g --no-audit --no-fund --prefix {}/{}{TMP_MARK}",
                c.versions_dir(&p.name).display(),
                p.version
            );
            let spec = format!(" {}@{}", p.package, p.version);
            assert!(
                log.iter()
                    .any(|l| l.starts_with(&prefix) && l.ends_with(&spec)),
                "{prefix}…{spec} not in {log:?}"
            );
            let dir = c.version_dir(&p.name, &p.version);
            assert!(dir.join(COMPLETE).is_file(), "{}", dir.display());
            assert_eq!(
                link(&c.bin_dir().join(&p.binary)),
                format!("../versions/{}/{}/bin/{}", p.name, p.version, p.binary)
            );
            assert_eq!(
                link(&c.dir.join(CURRENT_DIR).join(&p.name)),
                format!("../versions/{}/{}", p.name, p.version)
            );
            assert_eq!(c.versions(&p.name), vec![p.version.clone()]);
            // No temp dir is left.
            let names: Vec<_> = std::fs::read_dir(c.versions_dir(&p.name))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names.len(), 1, "{names:?}");
        }
        let m = c.read_manifest();
        assert_eq!(m.len(), c.pins.len());
        let codex = c.pin("codex").unwrap();
        assert_eq!(m["codex"].version, codex.version);
        assert_eq!(m["codex"].package, "@openai/codex");
        assert_eq!(m["codex"].versions, vec![codex.version.clone()]);
        assert!(r.extension.starts_with("installed into"), "{}", r.extension);
        let marker = std::fs::read_to_string(c.copilot_home.join(EXTENSION_MARKER)).unwrap();
        assert!(marker.starts_with("copilot-prompt-"), "{marker}");
        let stamp = m["codex"].installed_at;

        // The command runs through `bin/`, as on PATH.
        let out = std::process::Command::new(c.bin_dir().join("codex"))
            .arg("--version")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains(&codex.version));

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

    /// An active version stays: an update survives a restart. A pin with
    /// no active version comes back at the pin.
    #[tokio::test]
    async fn install_keeps_an_updated_version_and_restores_a_missing_one() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        c.install().await;
        let u = c.update("codex", "latest", false).await.unwrap();
        assert_eq!(u.to, "9.0.0");
        std::fs::remove_file(c.dir.join(CURRENT_DIR).join("pi")).unwrap();
        // A manual npm run replaced `bin/codex`: the install points it back.
        std::fs::remove_file(c.bin_dir().join("codex")).unwrap();
        std::os::unix::fs::symlink(
            "../lib/node_modules/@openai/codex/x",
            c.bin_dir().join("codex"),
        )
        .unwrap();
        let r = c.install().await;
        assert_eq!(
            link(&c.bin_dir().join("codex")),
            "../versions/codex/9.0.0/bin/codex"
        );
        let item = |n: &str| r.items.iter().find(|i| i.name == n).unwrap().clone();
        assert_eq!(item("codex").action, "present");
        assert_eq!(item("codex").version.as_deref(), Some("9.0.0"));
        // The pi version dir is complete on disk: the install reuses it.
        assert_eq!(item("pi").action, "installed");
        assert_eq!(c.read_manifest()["codex"].version, "9.0.0");
        assert!(active_version(&c.dir, "pi").is_some());
    }

    /// An update installs a new version dir and switches to it. The old
    /// dir stays, with its files, for an agent that runs it. A version
    /// that is complete on disk needs no npm run; `force` installs it
    /// again.
    #[tokio::test]
    async fn update_installs_a_version_dir_and_switches_to_it() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        c.install().await;
        let pin = c.pin("codex").unwrap().version.clone();
        // An agent resolved the active version before the update.
        let old = active_version(&c.dir, "codex").unwrap();
        assert_eq!(old.version, pin);
        assert_eq!(old.dir, c.version_dir("codex", &pin));

        let runs = npm_log(tmp.path()).len();
        let same = c.update("codex", &pin, false).await.unwrap();
        assert_eq!(
            (same.from.as_deref(), same.to.as_str(), same.reused),
            (Some(pin.as_str()), pin.as_str(), true)
        );
        assert!(same.output.contains("no npm run"), "{}", same.output);
        assert_eq!(npm_log(tmp.path()).len(), runs);

        let up = c.update("codex", "0.170.1", false).await.unwrap();
        assert_eq!(
            (up.from.as_deref(), up.to.as_str(), up.reused),
            (Some(pin.as_str()), "0.170.1", false)
        );
        assert!(up.output.contains("added 1 package"), "{}", up.output);
        assert_eq!(c.read_manifest()["codex"].version, "0.170.1");
        assert_eq!(
            c.read_manifest()["codex"].versions,
            vec![pin.clone(), "0.170.1".to_owned()]
        );
        assert_eq!(
            link(&c.bin_dir().join("codex")),
            "../versions/codex/0.170.1/bin/codex"
        );
        assert_eq!(active_version(&c.dir, "codex").unwrap().version, "0.170.1");
        // The old real path still has its files.
        let out = std::process::Command::new(old.dir.join("bin/codex"))
            .arg("--version")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains(&pin));

        // Back to the old version: no npm run.
        let runs = npm_log(tmp.path()).len();
        let back = c.update("codex", &pin, false).await.unwrap();
        assert!(back.reused);
        assert_eq!(npm_log(tmp.path()).len(), runs);
        assert_eq!(active_version(&c.dir, "codex").unwrap().version, pin);

        // `force`: npm installs it again; the replaced dir goes aside and
        // the GC removes it.
        let again = c.update("codex", &pin, true).await.unwrap();
        assert!(!again.reused);
        assert_eq!(npm_log(tmp.path()).len(), runs + 1);
        let aside: Vec<_> = std::fs::read_dir(c.versions_dir("codex"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.contains(OLD_MARK))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        c.gc(Some(&InUse::new()));
        assert_eq!(c.versions("codex"), vec![pin.clone()]);

        // A Copilot update reinstalls the extension.
        let cp = c.update("copilot", "1.0.200", false).await.unwrap();
        assert!(
            cp.output.contains("Copilot prompt extension: installed"),
            "{}",
            cp.output
        );
        let marker = std::fs::read_to_string(c.copilot_home.join(EXTENSION_MARKER)).unwrap();
        assert!(marker.contains("copilot-1.0.200"), "{marker}");
    }

    /// A failed npm run (or a bad version) changes nothing: the active
    /// version, the symlinks, and the manifest stay, and no temp dir is
    /// left.
    #[tokio::test]
    async fn a_failed_install_leaves_the_active_version_as_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        c.install().await;
        let before = c.read_manifest();
        let bin = link(&c.bin_dir().join("codex"));
        let e = c.update("codex", "99.0.0", false).await.unwrap_err();
        assert!(e.message.contains("failed (exit 1)"), "{}", e.message);
        assert!(e.output.contains("ETARGET"), "{}", e.output);
        assert_eq!(
            e.from.as_deref(),
            Some(c.pin("codex").unwrap().version.as_str())
        );
        assert_eq!(c.read_manifest(), before);
        assert_eq!(link(&c.bin_dir().join("codex")), bin);
        assert_eq!(
            std::fs::read_dir(c.versions_dir("codex")).unwrap().count(),
            1,
            "no temp dir is left"
        );

        let runs = npm_log(tmp.path()).len();
        let e = c.update("codex", "--global", false).await.unwrap_err();
        assert!(e.message.contains("not `latest`"), "{}", e.message);
        assert!(c.update("nope", "latest", false).await.is_err());
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
        assert!(c.versions("codex").is_empty());
    }

    /// The GC keeps the active version and each version that a live agent
    /// runs, and removes the rest. Without the in-use list it keeps the
    /// newest, up to `KEEP_VERSIONS`. A stale temp dir goes too.
    #[tokio::test]
    async fn gc_keeps_the_active_and_the_used_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        c.install().await;
        let pin = c.pin("codex").unwrap().version.clone();
        for v in ["0.170.0", "0.171.0", "0.172.0", "0.173.0"] {
            c.update("codex", v, false).await.unwrap();
        }
        assert_eq!(c.versions("codex").len(), 5);

        // Without the in-use list: the newest three.
        let removed = c.gc(None);
        assert_eq!(
            removed,
            vec![format!("codex {pin}"), "codex 0.170.0".to_owned()]
        );
        assert_eq!(c.versions("codex"), ["0.171.0", "0.172.0", "0.173.0"]);

        // With it: the active one and the used ones.
        let mut used = InUse::new();
        used.entry("codex".into())
            .or_default()
            .insert("0.171.0".into());
        let removed = c.gc(Some(&used));
        assert_eq!(removed, vec!["codex 0.172.0".to_owned()]);
        assert_eq!(c.versions("codex"), ["0.171.0", "0.173.0"]);
        assert_eq!(c.read_manifest()["codex"].versions, ["0.171.0", "0.173.0"]);
        // The other pins keep their one active version.
        assert_eq!(c.versions("pi").len(), 1);

        // A fresh temp dir stays (an install may run); a version dir with
        // no marker goes.
        let fresh = c.versions_dir("codex").join(format!("1.0.0{TMP_MARK}x"));
        let broken = c.versions_dir("codex").join("0.180.0");
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::create_dir_all(broken.join("bin")).unwrap();
        c.gc(Some(&used));
        assert!(fresh.is_dir());
        assert!(!broken.exists());
        // The active version is never removed.
        c.gc(Some(&InUse::new()));
        assert_eq!(c.versions("codex"), ["0.173.0"]);
    }

    /// The flat layout of before the version dirs: npm's own prefix with
    /// `lib/node_modules/<package>` and `bin/<binary>` links into it.
    fn flat_layout(c: &ToolsConfig) {
        for p in &c.pins {
            let pkg = c.dir.join(FLAT_MODULES).join(&p.package);
            std::fs::create_dir_all(pkg.join("bin")).unwrap();
            std::fs::create_dir_all(pkg.join("node_modules/.bin")).unwrap();
            std::fs::write(
                pkg.join("package.json"),
                format!(r#"{{"name":"{}","version":"{}"}}"#, p.package, p.version),
            )
            .unwrap();
            let main = pkg.join("bin/main");
            std::fs::write(
                &main,
                format!("#!/bin/sh\necho \"{} CLI {}\"\n", p.binary, p.version),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&main, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("../../bin/main", pkg.join("node_modules/.bin/x")).unwrap();
            std::fs::create_dir_all(c.bin_dir()).unwrap();
            std::os::unix::fs::symlink(
                Path::new("../lib/node_modules")
                    .join(&p.package)
                    .join("bin/main"),
                c.bin_dir().join(&p.binary),
            )
            .unwrap();
        }
    }

    /// The install moves the flat layout into version dirs with hard links
    /// and no npm run. The flat files stay until `remove_flat`, so an agent
    /// that started on them keeps them.
    #[tokio::test]
    async fn install_moves_the_flat_layout_into_version_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        flat_layout(&c);
        assert!(c.has_flat_layout());
        let r = c.install().await;
        assert_eq!(r.failed(), 0, "{r:#?}");
        assert!(r.items.iter().all(|i| i.action == "migrated"), "{r:#?}");
        assert!(npm_log(tmp.path()).is_empty(), "no npm run");
        let codex = c.pin("codex").unwrap();
        let dir = c.version_dir("codex", &codex.version);
        assert!(dir.join(COMPLETE).is_file());
        assert_eq!(
            link(&c.bin_dir().join("codex")),
            format!("../versions/codex/{}/bin/codex", codex.version)
        );
        assert_eq!(
            link(&dir.join("bin/codex")),
            "../lib/node_modules/@openai/codex/bin/main"
        );
        assert_eq!(
            link(&dir.join("lib/node_modules/@openai/codex/node_modules/.bin/x")),
            "../../bin/main"
        );
        // A hard link: the same file as the flat one.
        use std::os::unix::fs::MetadataExt;
        let flat = c.dir.join(FLAT_MODULES).join("@openai/codex/bin/main");
        let moved = dir.join("lib/node_modules/@openai/codex/bin/main");
        assert_eq!(
            std::fs::metadata(&flat).unwrap().ino(),
            std::fs::metadata(&moved).unwrap().ino()
        );
        let out = std::process::Command::new(c.bin_dir().join("codex"))
            .arg("--version")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains(&codex.version));
        assert!(flat.is_file(), "the flat files stay");
        assert_eq!(c.read_manifest()["codex"].version, codex.version);

        // A second install finds the version dirs.
        let again = c.install().await;
        assert!(again.items.iter().all(|i| i.action == "present"));

        assert!(c.remove_flat().unwrap());
        assert!(!c.has_flat_layout() && !c.dir.join("lib").exists());
        assert!(moved.is_file(), "the version dir keeps its files");
        assert!(!c.remove_flat().unwrap());
    }

    /// A flat command that is not a link into its package is not moved:
    /// npm installs that pin into a version dir.
    #[tokio::test]
    async fn a_flat_command_outside_its_package_gets_a_fresh_install() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        flat_layout(&c);
        let bin = c.bin_dir().join("pi");
        std::fs::remove_file(&bin).unwrap();
        std::os::unix::fs::symlink("/usr/bin/true", &bin).unwrap();
        let r = c.install().await;
        let pi = r.items.iter().find(|i| i.name == "pi").unwrap();
        assert_eq!(pi.action, "installed", "{r:#?}");
        assert_eq!(npm_log(tmp.path()).len(), 1);
        assert!(link(&bin).starts_with("../versions/pi/"));
    }
}
