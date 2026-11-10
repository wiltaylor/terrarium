//! Backup and restore of the daemon's data into zip files (docs/api.md
//! §32, PLAN §23.8).
//!
//! A backup is one zip file in the backup dir (`TERRARIUM_BACKUP_DIR`,
//! default `<state>/backups`). It holds:
//!
//! | Entry | Content |
//! |---|---|
//! | `README.txt` | A warning: the file holds secrets. Always plain. |
//! | `backup.json` | The [`Header`]: the ID, the kind, the base, the seal parameters. Always plain. |
//! | `manifest.json` | The [`Manifest`]: every file, dir, and symlink of the roots, with its SHA-256. Sealed when the backup is. |
//! | `files/<root>/<path>` | The content of each file that this backup stores. Sealed when the backup is. |
//!
//! The roots are `state` (the state dir: the database, the agent dirs,
//! the credentials, `secrets.key`), `config` (the config repo, with its
//! `.git`), and `work` (the agent workspaces, only with `--work`). The
//! database goes in as a consistent copy (`VACUUM INTO`), never as the raw
//! file: a copy of a WAL database while the daemon writes can be torn. The
//! walk skips the backup dir, the live database files, and dirs named
//! `target`, `node_modules`, and `.cache` (build output and caches). The
//! tools dir is not a root.
//!
//! A **full** backup stores every file. A **diff** backup names a full
//! backup as its base and stores only the files that changed since that
//! base. Its manifest still lists every file, so a restore of a diff
//! (with its base) makes the exact tree of the time of the diff: files
//! that are gone are not restored. The database is in each backup.
//!
//! A restore checks the manifest and each SHA-256 while it writes into a
//! staging dir in each root. Only when every file is good does it move the
//! old content of each root to `<root>/.terrarium-pre-restore-<time>/`
//! and the new content into place. The daemon must be stopped
//! (`terrariumd backup restore` refuses while a daemon answers on the
//! socket or holds the state lock, and holds that lock while it runs).

pub mod schedule;
pub mod seal;

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub use seal::{Encryption, SealKey};

/// The value of [`Header::format`].
pub const FORMAT: &str = "terrarium-backup";
/// The format version that this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// The env var of the backup dir.
pub const ENV_DIR: &str = "TERRARIUM_BACKUP_DIR";
/// The backup dir in the state dir, when [`ENV_DIR`] is not set.
pub const DEFAULT_SUBDIR: &str = "backups";
pub const HEADER: &str = "backup.json";
pub const MANIFEST: &str = "manifest.json";
pub const README: &str = "README.txt";
/// The prefix of the content entries.
pub const FILES: &str = "files/";
/// The database file in the state dir.
pub const DB_FILE: &str = "terrarium.db";
/// Dir names that the walk skips at any depth: build output and caches.
pub const SKIP_DIRS: &[&str] = &["target", "node_modules", ".cache"];
/// The staging dir of a restore, in each root.
pub const STAGING_PREFIX: &str = ".terrarium-restore-";
/// The old content of a root after a restore.
pub const PRE_RESTORE_PREFIX: &str = ".terrarium-pre-restore-";

const README_TEXT: &str = "\
This is a Terrarium backup (terrariumd backup).

IT HOLDS SECRETS. The state root has the secret store key (secrets.key),
the database with the sealed secrets, the plan logins of the accounts,
the git credentials, and the session key. Anyone with this file and no
passphrase can read all of them. Keep it as you keep the server itself.

backup.json says whether a passphrase seals the files (\"encryption\").
Restore with:   terrariumd backup restore <this file>
Check with:     terrariumd backup verify <this file>
Drill with:     terrariumd backup drill <this file>  (a restore into a scratch dir)
";

/// A full or a diff backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Every file.
    Full,
    /// The files that changed since the last full backup.
    Diff,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Diff => "diff",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The full backup that a diff needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaseRef {
    pub id: String,
    /// The file name of the base at the time of the diff.
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// `backup.json`: the plain header of a backup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Header {
    /// [`FORMAT`].
    pub format: String,
    /// [`FORMAT_VERSION`].
    pub version: u32,
    /// A ULID.
    pub id: String,
    pub kind: Kind,
    pub created_at: DateTime<Utc>,
    /// The base of a diff. `None` for a full backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<BaseRef>,
    /// The version of terrariumd that made the backup.
    pub terrarium_version: String,
    /// The schema version (`PRAGMA user_version`) of the database.
    pub db_user_version: i64,
    /// Always `true`: the state root holds the keys of the secret store.
    pub contains_secrets: bool,
    /// The roots in the backup: `state`, `config`, `work`.
    pub roots: Vec<String>,
    /// The seal parameters, when a passphrase seals the backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<Encryption>,
    /// The entries of the manifest.
    pub entries: usize,
    /// The files whose content this zip stores.
    pub stored_files: usize,
    /// The plain bytes of those files.
    pub stored_bytes: u64,
}

/// The type of a manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// One file, dir, or symlink.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// `<root>/<relative path>`, with `/`.
    pub path: String,
    pub kind: EntryKind,
    /// The permission bits.
    pub mode: u32,
    /// Files: the size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Files: the modification time, nanoseconds since the epoch.
    #[serde(default)]
    pub mtime_ns: i128,
    /// Files: the SHA-256 of the content, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Symlinks: the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Files: the ID of the backup whose zip stores the content (this
    /// backup, or the base of a diff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_in: Option<String>,
}

/// `manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub entries: Vec<Entry>,
}

/// The dirs to back up or to restore into.
#[derive(Debug, Clone)]
pub struct Roots {
    pub state: PathBuf,
    pub config: Option<PathBuf>,
    pub work: Option<PathBuf>,
}

impl Roots {
    fn dir(&self, root: &str) -> Option<&Path> {
        match root {
            "state" => Some(&self.state),
            "config" => self.config.as_deref(),
            "work" => self.work.as_deref(),
            _ => None,
        }
    }
}

/// Where the database copy comes from.
#[derive(Debug, Clone)]
pub enum DbSource {
    /// The database file, with no daemon: the create copies it with
    /// `VACUUM INTO`.
    File(PathBuf),
    /// A consistent copy that the caller made (the daemon makes it with
    /// [`vacuum_into`] on its own connection).
    Snapshot(PathBuf),
}

/// The settings of a create.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub kind: Kind,
    /// The backup dir.
    pub dir: PathBuf,
    /// The base of a diff. Default: the newest full backup in `dir`.
    pub base: Option<PathBuf>,
    /// Seal the backup with this passphrase.
    pub passphrase: Option<String>,
    pub db: DbSource,
}

/// The result of a create.
#[derive(Debug, Clone, Serialize)]
pub struct Created {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub header: Header,
}

/// The backup dir: [`ENV_DIR`] when set, else `<state>/backups`.
pub fn default_dir(state_dir: &Path) -> PathBuf {
    std::env::var_os(ENV_DIR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join(DEFAULT_SUBDIR))
}

/// Copy the database of `conn` to `dest` with `VACUUM INTO`: one
/// consistent snapshot, also while other connections write.
pub fn vacuum_into(conn: &rusqlite::Connection, dest: &Path) -> rusqlite::Result<()> {
    let dest = dest.to_string_lossy().into_owned();
    conn.execute("VACUUM INTO ?1", [dest])?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A reader that hashes what goes through it.
struct HashRead<R> {
    inner: R,
    hash: Sha256,
    len: u64,
}

impl<R: Read> Read for HashRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
}

impl<R> HashRead<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            len: 0,
        }
    }

    fn done(self) -> (String, u64) {
        (hex(&self.hash.finalize()), self.len)
    }
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut r = HashRead::new(BufReader::new(File::open(path)?));
    io::copy(&mut r, &mut io::sink())?;
    Ok(r.done().0)
}

fn mtime_ns(meta: &fs::Metadata) -> i128 {
    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())
}

/// A path of the walk: the entry and the file it comes from.
struct Item {
    entry: Entry,
    source: PathBuf,
}

/// Walk `dir` as the root `root`, in path order.
fn walk(root: &str, dir: &Path, skip: &HashSet<PathBuf>, out: &mut Vec<Item>) -> io::Result<()> {
    fn rec(
        root: &str,
        base: &Path,
        dir: &Path,
        skip: &HashSet<PathBuf>,
        out: &mut Vec<Item>,
    ) -> io::Result<()> {
        let mut names: Vec<_> = fs::read_dir(dir)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let name_s = name.to_string_lossy();
            let top = dir == base;
            if skip.contains(&path)
                || (top
                    && (name_s.starts_with(STAGING_PREFIX)
                        || name_s.starts_with(PRE_RESTORE_PREFIX)))
            {
                continue;
            }
            let Ok(rel) = path.strip_prefix(base) else {
                continue;
            };
            let Some(rel) = rel.to_str() else {
                tracing::warn!(path = %path.display(), "backup skips a path that is not UTF-8");
                continue;
            };
            let meta = fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            let mut entry = Entry {
                path: format!("{root}/{rel}"),
                kind: EntryKind::File,
                mode: meta.permissions().mode() & 0o7777,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link: None,
                stored_in: None,
            };
            if ft.is_dir() {
                if SKIP_DIRS.contains(&name_s.as_ref()) {
                    continue;
                }
                entry.kind = EntryKind::Dir;
                out.push(Item {
                    entry,
                    source: path.clone(),
                });
                rec(root, base, &path, skip, out)?;
            } else if ft.is_symlink() {
                let target = fs::read_link(&path)?;
                entry.kind = EntryKind::Symlink;
                entry.link = Some(target.to_string_lossy().into_owned());
                out.push(Item {
                    entry,
                    source: path,
                });
            } else if ft.is_file() {
                entry.size = meta.len();
                entry.mtime_ns = mtime_ns(&meta);
                out.push(Item {
                    entry,
                    source: path,
                });
            }
            // Sockets, fifos, and devices: not data.
        }
        Ok(())
    }
    rec(root, dir, dir, skip, out)
}

fn canonical(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// A free name in `dir` for a backup of `kind` made at `at`.
fn file_name(dir: &Path, kind: Kind, at: DateTime<Utc>) -> String {
    let stamp = at.format("%Y%m%dT%H%M%SZ");
    let mut name = format!("terrarium-backup-{stamp}-{kind}.zip");
    let mut n = 2;
    while dir.join(&name).exists() {
        name = format!("terrarium-backup-{stamp}-{kind}-{n}.zip");
        n += 1;
    }
    name
}

/// Make a backup of `roots` in `opts.dir`. Blocking: call it from a
/// blocking thread in the daemon.
pub fn create(roots: &Roots, opts: &CreateOptions) -> anyhow::Result<Created> {
    let dir = &opts.dir;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot set the mode of {}", dir.display()))?;
    let id = ulid::Ulid::new().to_string();
    let created_at = Utc::now();

    // The base of a diff, with the entries of its manifest.
    let mut base_ref = None;
    let mut base_entries: BTreeMap<String, Entry> = BTreeMap::new();
    if opts.kind == Kind::Diff {
        let path = match &opts.base {
            Some(p) => p.clone(),
            None => newest_full(dir)?.with_context(|| {
                format!(
                    "no full backup in {}: make a full backup first",
                    dir.display()
                )
            })?,
        };
        let mut base = Archive::open(&path, opts.passphrase.as_deref())?;
        if base.header.kind != Kind::Full {
            bail!("the base {} is not a full backup", path.display());
        }
        let m = base.manifest()?;
        base_ref = Some(BaseRef {
            id: base.header.id.clone(),
            name: file_name_of(&path),
            created_at: base.header.created_at,
        });
        base_entries = m.entries.into_iter().map(|e| (e.path.clone(), e)).collect();
    }

    let tmp = tempfile::Builder::new()
        .prefix(".backup-")
        .tempdir_in(dir)
        .with_context(|| format!("cannot make a temp dir in {}", dir.display()))?;
    let snapshot = match &opts.db {
        DbSource::Snapshot(p) => p.clone(),
        DbSource::File(live) => {
            let dest = tmp.path().join(DB_FILE);
            if live.exists() {
                let conn = rusqlite::Connection::open_with_flags(
                    live,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .with_context(|| format!("cannot open {}", live.display()))?;
                conn.busy_timeout(std::time::Duration::from_secs(30))?;
                vacuum_into(&conn, &dest)
                    .with_context(|| format!("cannot copy {}", live.display()))?;
            }
            dest
        }
    };
    let db_user_version: i64 = if snapshot.exists() {
        let c = rusqlite::Connection::open_with_flags(
            &snapshot,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        c.pragma_query_value(None, "user_version", |r| r.get(0))?
    } else {
        0
    };

    // The walk. The database copy stands in for the live files. The walk
    // reads canonical paths, so the skip set holds canonical paths.
    let state_c = canonical(&roots.state);
    let mut skip: HashSet<PathBuf> = HashSet::new();
    skip.insert(canonical(dir));
    // Reconstructible image payloads, not application data. In particular do
    // not copy every retained daemon binary into every pre-upgrade backup.
    skip.insert(state_c.join("releases"));
    // The one-shot helper files of live git and ssh runs (askpass
    // scripts, keys): never data, and secrets in the clear.
    skip.insert(crate::secrets::RunDirs::base(&state_c));
    for f in ["", "-wal", "-shm", "-journal"] {
        skip.insert(state_c.join(format!("{DB_FILE}{f}")));
    }
    let mut items = Vec::new();
    let mut root_names = Vec::new();
    for (root, path) in [
        ("state", Some(roots.state.as_path())),
        ("config", roots.config.as_deref()),
        ("work", roots.work.as_deref()),
    ] {
        let Some(path) = path else { continue };
        if !path.is_dir() {
            if root == "state" {
                bail!("the state dir {} does not exist", path.display());
            }
            tracing::warn!(root, dir = %path.display(), "backup skips a root that does not exist");
            continue;
        }
        let path = canonical(path);
        root_names.push(root.to_owned());
        if root == "state" && snapshot.exists() {
            let meta = fs::metadata(&snapshot)?;
            items.push(Item {
                entry: Entry {
                    path: format!("state/{DB_FILE}"),
                    kind: EntryKind::File,
                    mode: 0o600,
                    size: meta.len(),
                    mtime_ns: mtime_ns(&meta),
                    sha256: None,
                    link: None,
                    stored_in: None,
                },
                source: snapshot.clone(),
            });
        }
        walk(root, &path, &skip, &mut items)
            .with_context(|| format!("cannot read {}", path.display()))?;
    }

    let key = match &opts.passphrase {
        Some(p) => Some(SealKey::create(p, &id)?),
        None => None,
    };
    let name = file_name(dir, opts.kind, created_at);
    let partial = dir.join(format!(".{name}.partial"));
    let out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("cannot create {}", partial.display()))?;
    let result = (|| -> anyhow::Result<Header> {
        let mut zip = ZipWriter::new(io::BufWriter::new(out));
        let plain = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o600);
        let stored = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o600);
        zip.start_file(README, plain)?;
        zip.write_all(README_TEXT.as_bytes())?;

        let mut entries = Vec::with_capacity(items.len());
        let (mut stored_files, mut stored_bytes) = (0usize, 0u64);
        for Item { mut entry, source } in items {
            if entry.kind != EntryKind::File {
                entries.push(entry);
                continue;
            }
            let base = base_entries
                .get(&entry.path)
                .filter(|b| b.kind == EntryKind::File);
            if let Some(b) = base {
                let same_stat =
                    b.size == entry.size && b.mtime_ns == entry.mtime_ns && b.mode == entry.mode;
                let same = same_stat
                    || hash_file(&source)
                        .map(|h| Some(h) == b.sha256)
                        .unwrap_or(false);
                if same {
                    entry.sha256 = b.sha256.clone();
                    entry.stored_in = b.stored_in.clone();
                    entries.push(entry);
                    continue;
                }
            }
            let file = match File::open(&source) {
                Ok(f) => f,
                // A file that went away during the walk (a log rotation).
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot read {}", source.display()));
                }
            };
            let large = entry.size > u64::from(u32::MAX) / 2;
            let zname = format!("{FILES}{}", entry.path);
            let mut r = HashRead::new(BufReader::new(file));
            match &key {
                None => {
                    zip.start_file(&zname, plain.large_file(large))?;
                    io::copy(&mut r, &mut zip)
                        .with_context(|| format!("cannot read {}", source.display()))?;
                }
                Some((k, _)) => {
                    zip.start_file(&zname, stored.large_file(large))?;
                    let sw = k.writer(&mut zip, &zname)?;
                    let mut enc = DeflateEncoder::new(sw, Compression::default());
                    io::copy(&mut r, &mut enc)
                        .with_context(|| format!("cannot read {}", source.display()))?;
                    enc.finish()?.finish()?;
                }
            }
            let (sha, len) = r.done();
            entry.size = len;
            entry.sha256 = Some(sha);
            entry.stored_in = Some(id.clone());
            stored_files += 1;
            stored_bytes += len;
            entries.push(entry);
        }

        let manifest = Manifest {
            id: id.clone(),
            entries,
        };
        let m = serde_json::to_vec(&manifest)?;
        match &key {
            None => {
                zip.start_file(MANIFEST, plain)?;
                zip.write_all(&m)?;
            }
            Some((k, _)) => {
                zip.start_file(MANIFEST, stored)?;
                let mut sw = k.writer(&mut zip, MANIFEST)?;
                sw.write_all(&m)?;
                sw.finish()?;
            }
        }
        let header = Header {
            format: FORMAT.into(),
            version: FORMAT_VERSION,
            id: id.clone(),
            kind: opts.kind,
            created_at,
            base: base_ref.clone(),
            terrarium_version: terrarium_version::FULL.into(),
            db_user_version,
            contains_secrets: true,
            roots: root_names.clone(),
            encryption: key.as_ref().map(|(_, e)| e.clone()),
            entries: manifest.entries.len(),
            stored_files,
            stored_bytes,
        };
        zip.start_file(HEADER, plain)?;
        zip.write_all(&serde_json::to_vec_pretty(&header)?)?;
        let mut w = zip.finish()?;
        w.flush()?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        Ok(header)
    })();
    let header = match result {
        Ok(h) => h,
        Err(e) => {
            let _ = fs::remove_file(&partial);
            return Err(e);
        }
    };
    let path = dir.join(&name);
    fs::rename(&partial, &path).with_context(|| format!("cannot write {}", path.display()))?;
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Created {
        name,
        path,
        size,
        header,
    })
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The error of a sealed backup that needs a passphrase.
#[derive(Debug, thiserror::Error)]
#[error(
    "backup {0} is sealed with a passphrase: give it with --passphrase-file or --passphrase-stdin"
)]
pub struct NeedsPassphrase(pub String);

/// An open backup file.
pub struct Archive {
    pub path: PathBuf,
    pub header: Header,
    zip: ZipArchive<BufReader<File>>,
    key: Option<SealKey>,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("path", &self.path)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

fn read_header_of(zip: &mut ZipArchive<BufReader<File>>, path: &Path) -> anyhow::Result<Header> {
    let mut f = zip
        .by_name(HEADER)
        .with_context(|| format!("{} is not a Terrarium backup (no {HEADER})", path.display()))?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    let h: Header =
        serde_json::from_str(&s).with_context(|| format!("bad {HEADER} in {}", path.display()))?;
    if h.format != FORMAT {
        bail!("{} is not a Terrarium backup", path.display());
    }
    if h.version > FORMAT_VERSION {
        bail!(
            "{} has backup format {}; this terrariumd reads {FORMAT_VERSION} at most",
            path.display(),
            h.version
        );
    }
    Ok(h)
}

/// The header of a backup file, with no passphrase.
pub fn read_header(path: &Path) -> anyhow::Result<Header> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut zip = ZipArchive::new(BufReader::new(f))
        .with_context(|| format!("{} is not a zip file", path.display()))?;
    read_header_of(&mut zip, path)
}

impl Archive {
    /// Open a backup. A sealed backup needs `passphrase`
    /// ([`NeedsPassphrase`] without it).
    pub fn open(path: &Path, passphrase: Option<&str>) -> anyhow::Result<Self> {
        let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut zip = ZipArchive::new(BufReader::new(f))
            .with_context(|| format!("{} is not a zip file", path.display()))?;
        let header = read_header_of(&mut zip, path)?;
        let key = match (&header.encryption, passphrase) {
            (None, _) => None,
            (Some(_), None) => return Err(NeedsPassphrase(file_name_of(path)).into()),
            (Some(e), Some(p)) => Some(SealKey::open(p, &header.id, e)?),
        };
        Ok(Self {
            path: path.to_path_buf(),
            header,
            zip,
            key,
        })
    }

    fn reader(&mut self, name: &str) -> anyhow::Result<Box<dyn Read + '_>> {
        let f = self
            .zip
            .by_name(name)
            .with_context(|| format!("{} has no entry {name}", self.path.display()))?;
        Ok(match &self.key {
            None => Box::new(f),
            Some(k) if name == MANIFEST => Box::new(k.reader(f, name)),
            Some(k) => Box::new(DeflateDecoder::new(k.reader(f, name))),
        })
    }

    /// The manifest.
    pub fn manifest(&mut self) -> anyhow::Result<Manifest> {
        let mut s = Vec::new();
        self.reader(MANIFEST)?.read_to_end(&mut s)?;
        let m: Manifest = serde_json::from_slice(&s)
            .with_context(|| format!("bad {MANIFEST} in {}", self.path.display()))?;
        if m.id != self.header.id {
            bail!(
                "the manifest of {} names another backup",
                self.path.display()
            );
        }
        Ok(m)
    }

    /// Copy the content of the file entry `e` into `out`, and check its
    /// size and SHA-256.
    fn copy_file(&mut self, e: &Entry, out: &mut dyn Write) -> anyhow::Result<()> {
        let name = format!("{FILES}{}", e.path);
        let mut r = HashRead::new(self.reader(&name)?);
        io::copy(&mut r, out).with_context(|| format!("cannot read {name}"))?;
        let (sha, len) = r.done();
        if len != e.size || Some(&sha) != e.sha256.as_ref() {
            bail!(
                "{}: {} does not match the manifest (checksum or size)",
                self.path.display(),
                e.path
            );
        }
        Ok(())
    }
}

/// One file of [`list`].
#[derive(Debug, Clone, Serialize)]
pub struct Listed {
    pub name: String,
    pub size: u64,
    #[serde(flatten)]
    pub header: Option<Header>,
    /// Why the header does not read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The backups in `dir`, oldest first. A missing dir is empty.
pub fn list(dir: &Path) -> anyhow::Result<Vec<Listed>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut out = Vec::new();
    for e in rd {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_backup_name(&name) {
            continue;
        }
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        let (header, error) = match read_header(&e.path()) {
            Ok(h) => (Some(h), None),
            Err(err) => (None, Some(format!("{err:#}"))),
        };
        out.push(Listed {
            name,
            size,
            header,
            error,
        });
    }
    out.sort_by(|a, b| {
        let ka = a.header.as_ref().map(|h| h.created_at);
        let kb = b.header.as_ref().map(|h| h.created_at);
        ka.cmp(&kb).then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// A file name that [`list`] shows and the API serves: a plain name that
/// ends in `.zip` and does not start with a dot.
pub fn is_backup_name(name: &str) -> bool {
    name.ends_with(".zip")
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && name.len() > 4
}

/// The newest full backup in `dir`.
pub fn newest_full(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    Ok(list(dir)?
        .into_iter()
        .rev()
        .find(|l| l.header.as_ref().is_some_and(|h| h.kind == Kind::Full))
        .map(|l| dir.join(l.name)))
}

/// The backup with the ID `id` in `dir`.
pub fn find_by_id(dir: &Path, id: &str) -> anyhow::Result<Option<PathBuf>> {
    Ok(list(dir)?
        .into_iter()
        .find(|l| l.header.as_ref().is_some_and(|h| h.id == id))
        .map(|l| dir.join(l.name)))
}

/// Open the base of the diff `diff`: `base` when given, else the backup
/// in the same dir with the base ID.
fn open_base(
    diff: &Archive,
    base: Option<&Path>,
    passphrase: Option<&str>,
) -> anyhow::Result<Archive> {
    let Some(r) = &diff.header.base else {
        bail!("the diff {} names no base", diff.path.display());
    };
    let path = match base {
        Some(p) => p.to_path_buf(),
        None => {
            let dir = diff.path.parent().unwrap_or(Path::new("."));
            find_by_id(dir, &r.id)?.with_context(|| {
                format!(
                    "the base {} ({}) of the diff is not in {}: give it with --base",
                    r.name,
                    r.id,
                    dir.display()
                )
            })?
        }
    };
    let a = Archive::open(&path, passphrase)?;
    if a.header.id != r.id {
        bail!(
            "{} is backup {}, but the diff needs {} ({})",
            path.display(),
            a.header.id,
            r.id,
            r.name
        );
    }
    if a.header.kind != Kind::Full {
        bail!("the base {} is not a full backup", path.display());
    }
    Ok(a)
}

/// The result of [`verify`].
#[derive(Debug, Clone, Serialize)]
pub struct Verified {
    pub id: String,
    pub kind: Kind,
    pub entries: usize,
    /// The files whose content this check read and hashed.
    pub checked_files: usize,
    /// The base of a diff, when the check had it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// Read each file of a backup (and of its base, for a diff) and check it
/// against the manifest.
pub fn verify(
    path: &Path,
    base: Option<&Path>,
    passphrase: Option<&str>,
) -> anyhow::Result<Verified> {
    let mut a = Archive::open(path, passphrase)?;
    let m = a.manifest()?;
    check_paths(&m)?;
    let mut b = match a.header.kind {
        Kind::Full => None,
        Kind::Diff => Some(open_base(&a, base, passphrase)?),
    };
    let mut checked = 0;
    for e in m.entries.iter().filter(|e| e.kind == EntryKind::File) {
        let src = source_of(&mut a, b.as_mut(), e)?;
        src.copy_file(e, &mut io::sink())?;
        checked += 1;
    }
    Ok(Verified {
        id: a.header.id.clone(),
        kind: a.header.kind,
        entries: m.entries.len(),
        checked_files: checked,
        base: b.map(|b| b.header.id.clone()),
    })
}

fn source_of<'a>(
    a: &'a mut Archive,
    b: Option<&'a mut Archive>,
    e: &Entry,
) -> anyhow::Result<&'a mut Archive> {
    let held = e.stored_in.as_deref().unwrap_or_default();
    if held == a.header.id {
        return Ok(a);
    }
    match b {
        Some(b) if b.header.id == held => Ok(b),
        _ => bail!("{} is in backup {held}, which is not here", e.path),
    }
}

/// Each path is `<known root>/<relative path>` in plain form (no empty,
/// `.`, or `..` part), no path is in the manifest twice, and no path is
/// below a symlink entry: the restore would write through the link, out of
/// the root.
fn check_paths(m: &Manifest) -> anyhow::Result<()> {
    for e in &m.entries {
        let (root, rel) = split(&e.path)?;
        if !matches!(root, "state" | "config" | "work") {
            bail!("unknown root in {}", e.path);
        }
        let ok = !rel.is_empty()
            && rel.split('/').all(|s| !matches!(s, "" | "." | ".."))
            && Path::new(rel)
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
        if !ok {
            bail!("bad path in the manifest: {}", e.path);
        }
    }
    // One entry per path: a second entry on the same path could act
    // through the first (a dir entry on a symlink entry sets the mode of
    // the link target).
    let mut seen: HashSet<&str> = HashSet::new();
    for e in &m.entries {
        if !seen.insert(e.path.as_str()) {
            bail!("bad manifest: the path {} is in it twice", e.path);
        }
    }
    // The paths are in plain form, so a string prefix is a path prefix.
    let links: HashSet<&str> = m
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Symlink)
        .map(|e| e.path.as_str())
        .collect();
    for e in &m.entries {
        let mut at = e.path.as_str();
        while let Some((parent, _)) = at.rsplit_once('/') {
            if links.contains(parent) {
                bail!(
                    "bad path in the manifest: {} is below the symlink {parent}",
                    e.path
                );
            }
            at = parent;
        }
    }
    Ok(())
}

fn split(path: &str) -> anyhow::Result<(&str, &str)> {
    path.split_once('/')
        .with_context(|| format!("bad path in the manifest: {path}"))
}

/// The settings of a restore.
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// The base of a diff. Default: the backup with the base ID in the
    /// dir of the diff.
    pub base: Option<PathBuf>,
    pub passphrase: Option<String>,
    /// Leave the work root as it is.
    pub skip_work: bool,
    /// Paths that stay where they are (the backup dir): a top-level entry
    /// of a root that holds one of them does not move.
    pub keep: Vec<PathBuf>,
}

/// One root of [`Restored`].
#[derive(Debug, Clone, Serialize)]
pub struct RestoredRoot {
    pub root: String,
    pub dir: PathBuf,
    /// The old content, when the root had any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<PathBuf>,
}

/// The result of a restore.
#[derive(Debug, Clone, Serialize)]
pub struct Restored {
    pub id: String,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub entries: usize,
    pub roots: Vec<RestoredRoot>,
}

/// Restore the backup `path` (a full, or a diff with its base) into
/// `roots`. The daemon must be stopped: the caller checks it.
pub fn restore(path: &Path, roots: &Roots, opts: &RestoreOptions) -> anyhow::Result<Restored> {
    let pass = opts.passphrase.as_deref();
    let mut a = Archive::open(path, pass)?;
    let m = a.manifest()?;
    check_paths(&m)?;
    let mut b = match a.header.kind {
        Kind::Full => None,
        Kind::Diff => Some(open_base(&a, opts.base.as_deref(), pass)?),
    };
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    for root in &a.header.roots {
        if root == "work" && opts.skip_work {
            continue;
        }
        let dir = roots.dir(root).with_context(|| {
            format!("the backup has the root `{root}`, but no dir is set for it")
        })?;
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        targets.push((root.clone(), canonical(dir)));
    }
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let staging_name = format!("{STAGING_PREFIX}{}", a.header.id);
    let staging = |dir: &Path| dir.join(&staging_name);
    let cleanup = |targets: &[(String, PathBuf)]| {
        for (_, dir) in targets {
            let _ = fs::remove_dir_all(staging(dir));
        }
    };
    let staged = (|| -> anyhow::Result<()> {
        for (_, dir) in &targets {
            let s = staging(dir);
            if s.exists() {
                fs::remove_dir_all(&s)?;
            }
            fs::create_dir(&s).with_context(|| format!("cannot create {}", s.display()))?;
            fs::set_permissions(&s, fs::Permissions::from_mode(0o700))?;
        }
        let mut dirs = Vec::new();
        for e in &m.entries {
            let (root, rel) = split(&e.path)?;
            let Some((_, dir)) = targets.iter().find(|(r, _)| r == root) else {
                continue;
            };
            let dest = staging(dir).join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            match e.kind {
                EntryKind::Dir => {
                    fs::create_dir_all(&dest)?;
                    dirs.push((dest, e.mode));
                }
                EntryKind::Symlink => {
                    let target = e.link.as_deref().unwrap_or_default();
                    std::os::unix::fs::symlink(target, &dest)
                        .with_context(|| format!("cannot make the symlink {}", e.path))?;
                }
                EntryKind::File => {
                    let mut f = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&dest)
                        .with_context(|| format!("cannot create {}", dest.display()))?;
                    {
                        let mut w = io::BufWriter::new(&mut f);
                        source_of(&mut a, b.as_mut(), e)?.copy_file(e, &mut w)?;
                        w.flush()?;
                    }
                    f.set_permissions(fs::Permissions::from_mode(e.mode))?;
                    if let Ok(secs) = i64::try_from(e.mtime_ns.div_euclid(1_000_000_000))
                        && let Ok(secs) = u64::try_from(secs)
                    {
                        let nanos =
                            u32::try_from(e.mtime_ns.rem_euclid(1_000_000_000)).unwrap_or_default();
                        let t = std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos);
                        let _ = f.set_modified(t);
                    }
                }
            }
        }
        // The dir modes last: a dir of mode 0500 takes no new file.
        for (d, mode) in dirs.iter().rev() {
            fs::set_permissions(d, fs::Permissions::from_mode(*mode))?;
        }
        Ok(())
    })();
    if let Err(e) = staged {
        cleanup(&targets);
        return Err(e.context("the restore stopped; the old data did not change"));
    }

    // Every file is good: swap each root.
    let mut keep: Vec<PathBuf> = opts.keep.iter().map(|p| canonical(p)).collect();
    keep.push(canonical(&roots.state).join("releases"));
    let mut done = Vec::new();
    for (root, dir) in &targets {
        let pre = dir.join(format!("{PRE_RESTORE_PREFIX}{stamp}"));
        let mut moved = false;
        for e in fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name == staging_name
                || name.starts_with(PRE_RESTORE_PREFIX)
                || name.starts_with(STAGING_PREFIX)
            {
                continue;
            }
            let p = e.path();
            if keep.iter().any(|k| k.starts_with(&p)) {
                continue;
            }
            if !moved {
                fs::create_dir(&pre).with_context(|| format!("cannot create {}", pre.display()))?;
                fs::set_permissions(&pre, fs::Permissions::from_mode(0o700))?;
                moved = true;
            }
            fs::rename(&p, pre.join(&name))
                .with_context(|| format!("cannot move {} aside", p.display()))?;
        }
        let s = staging(dir);
        for e in fs::read_dir(&s)? {
            let e = e?;
            let to = dir.join(e.file_name());
            if fs::symlink_metadata(&to).is_ok() {
                // A kept path (the backup dir) that the backup also has:
                // the live one stays.
                continue;
            }
            fs::rename(e.path(), &to)
                .with_context(|| format!("cannot move {} into place", to.display()))?;
        }
        let _ = fs::remove_dir_all(&s);
        done.push(RestoredRoot {
            root: root.clone(),
            dir: dir.clone(),
            previous: moved.then_some(pre),
        });
    }
    Ok(Restored {
        id: a.header.id.clone(),
        kind: a.header.kind,
        base: b.map(|b| b.header.id.clone()),
        entries: m.entries.len(),
        roots: done,
    })
}

/// Delete the old backups in `dir`: keep the newest `keep_full` full
/// backups and the diffs of them. Answers the deleted names.
///
/// `verified_full` is the ID of a full backup that a verify just passed.
/// It must be the newest full backup in `dir`: nothing is deleted before
/// a newer full backup is known to be good (docs/api.md §32.4).
pub fn prune(dir: &Path, keep_full: usize, verified_full: &str) -> anyhow::Result<Vec<String>> {
    let all = list(dir)?;
    let fulls: Vec<&Listed> = all
        .iter()
        .filter(|l| l.header.as_ref().is_some_and(|h| h.kind == Kind::Full))
        .collect();
    let newest = fulls
        .last()
        .and_then(|l| l.header.as_ref())
        .map(|h| h.id.as_str());
    if newest != Some(verified_full) {
        bail!(
            "no prune: the verified full backup {verified_full} is not the newest full backup in {}",
            dir.display()
        );
    }
    let keep_full = keep_full.max(1);
    if fulls.len() <= keep_full {
        return Ok(Vec::new());
    }
    let drop: HashSet<String> = fulls[..fulls.len() - keep_full]
        .iter()
        .filter_map(|l| l.header.as_ref().map(|h| h.id.clone()))
        .collect();
    let mut removed = Vec::new();
    for l in &all {
        let Some(h) = &l.header else { continue };
        let gone = drop.contains(&h.id) || h.base.as_ref().is_some_and(|b| drop.contains(&b.id));
        if gone {
            fs::remove_file(dir.join(&l.name))
                .with_context(|| format!("cannot delete {}", l.name))?;
            removed.push(l.name.clone());
        }
    }
    Ok(removed)
}

/// The prefix of a drill dir in the backup dir. The walk of a backup
/// skips the backup dir, and [`list`] skips names with a dot.
pub const DRILL_PREFIX: &str = ".drill-";

/// Refuse a drill dir that is not empty: the drill writes into it, and
/// the caller deletes it. Only an absent dir or an empty dir passes. A
/// dir that cannot be read (permission denied, not a dir) is refused: it
/// may hold files of its own. A symlink is refused: the restore would
/// write the secrets into its target, and the cleanup removes only the
/// link.
pub fn check_drill_dir(dir: &Path) -> anyhow::Result<()> {
    if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("the drill dir {} is a symlink", dir.display());
    }
    match fs::read_dir(dir) {
        Ok(mut d) => {
            if d.next().is_some() {
                bail!("the drill dir {} is not empty", dir.display());
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => bail!("cannot read the drill dir {}: {e}", dir.display()),
    }
}

/// The result of [`drill`].
#[derive(Debug, Clone, Serialize)]
pub struct Drilled {
    pub id: String,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub entries: usize,
    /// The restored files whose size matched the manifest after the swap.
    pub files: usize,
    /// The scratch dir with the roots `state`, `config`, `work`.
    pub dir: PathBuf,
    /// `PRAGMA user_version` of the restored database, the same as the
    /// header.
    pub db_user_version: i64,
    /// `PRAGMA integrity_check` of the restored database: `ok`.
    pub integrity: String,
    /// The restored state root has `secrets.key`.
    pub secrets_key: bool,
}

/// The restore drill (docs/api.md §32.3): restore `path` (and the base of
/// a diff) into the empty scratch dir `scratch` (`state`, `config`, and
/// `work` in it), then check the result: each file of the manifest is in
/// place with its size, the database opens with the schema version of the
/// header and passes `PRAGMA integrity_check`, and `secrets.key` is back
/// when the backup had it. The live data does not change. The caller
/// deletes `scratch`: it holds the secrets in plain form.
pub fn drill(
    path: &Path,
    base: Option<&Path>,
    passphrase: Option<&str>,
    scratch: &Path,
) -> anyhow::Result<Drilled> {
    check_drill_dir(scratch)?;
    fs::create_dir_all(scratch).with_context(|| format!("cannot create {}", scratch.display()))?;
    fs::set_permissions(scratch, fs::Permissions::from_mode(0o700))?;
    let header = read_header(path)?;
    let roots = Roots {
        state: scratch.join("state"),
        config: Some(scratch.join("config")),
        work: Some(scratch.join("work")),
    };
    let r = restore(
        path,
        &roots,
        &RestoreOptions {
            base: base.map(Path::to_path_buf),
            passphrase: passphrase.map(str::to_owned),
            skip_work: false,
            keep: Vec::new(),
        },
    )?;
    // The manifest again, for the check of the tree after the swap.
    let m = Archive::open(path, passphrase)?.manifest()?;
    let mut files = 0;
    let mut had_key = false;
    for e in &m.entries {
        let (root, rel) = split(&e.path)?;
        let Some(dir) = roots.dir(root) else { continue };
        let p = dir.join(rel);
        let meta = fs::symlink_metadata(&p)
            .with_context(|| format!("the drill: {} is not restored", e.path))?;
        match e.kind {
            EntryKind::File => {
                if !meta.is_file() || meta.len() != e.size {
                    bail!("the drill: {} does not match the manifest", e.path);
                }
                files += 1;
                had_key |= e.path == "state/secrets.key";
            }
            EntryKind::Dir if !meta.is_dir() => {
                bail!("the drill: {} is not a dir", e.path)
            }
            EntryKind::Symlink if !meta.file_type().is_symlink() => {
                bail!("the drill: {} is not a symlink", e.path)
            }
            _ => {}
        }
    }
    let db = roots.state.join(DB_FILE);
    let c = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("the drill: cannot open {}", db.display()))?;
    let integrity: String = c
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .context("the drill: the integrity check of the database failed")?;
    if integrity != "ok" {
        bail!("the drill: the restored database is damaged: {integrity}");
    }
    let user_version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if user_version != header.db_user_version {
        bail!(
            "the drill: the restored database has schema version {user_version}, the header {}",
            header.db_user_version
        );
    }
    let secrets_key = roots.state.join("secrets.key").is_file();
    if had_key && !secrets_key {
        bail!("the drill: secrets.key is not restored");
    }
    Ok(Drilled {
        id: r.id,
        kind: r.kind,
        base: r.base,
        entries: r.entries,
        files,
        dir: scratch.to_path_buf(),
        db_user_version: user_version,
        integrity,
        secrets_key,
    })
}

#[cfg(test)]
mod tests;
