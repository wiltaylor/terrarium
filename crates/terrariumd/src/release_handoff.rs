//! Private, daemon-owned release handoffs. Never served by public artifact URLs.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult};
use crate::store::jobs::{self, Job, JobKind, JobState};
use rusqlite::{Connection, OptionalExtension, params};

pub const DEFAULT_OUTPUT_LIMIT: u64 = 200 * 1024 * 1024;
pub const ATTACHMENT_LIMIT: usize = 1024 * 1024;
pub const CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Successor {
    pub predecessor: String,
    pub attachment_size: u64,
    pub attachment_sha256: String,
}

pub fn successor(c: &Connection, job: &str) -> ApiResult<Option<Successor>> {
    c.query_row("SELECT predecessor, attachment_size, attachment_sha256 FROM release_successors WHERE job=?1", [job], |r| {
        Ok(Successor { predecessor: r.get(0)?, attachment_size: r.get(1)?, attachment_sha256: r.get(2)? })
    }).optional().map_err(Into::into)
}

pub fn check_predecessor(c: &Connection, job: &Job) -> ApiResult<()> {
    if job.kind != JobKind::Release || job.state != JobState::Passed || successor(c, &job.id)?.is_some() {
        return Err(ApiError::conflict("--after must name a passed phase-1 release job"));
    }
    let active: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM release_successors s JOIN jobs j ON j.id=s.job WHERE s.predecessor=?1 AND j.state IN ('queued','running','passed'))", [&job.id], |r| r.get(0))?;
    if active { return Err(ApiError::conflict("a phase-2 job is already queued, running, or passed")); }
    Ok(())
}

pub fn link(c: &Connection, job: &Job, predecessor: &str) -> ApiResult<()> {
    let previous = jobs::get(c, predecessor)?;
    check_predecessor(c, &previous)?;
    if job.kind != JobKind::Release || job.project != previous.project || job.repo != previous.repo || job.commit != previous.commit {
        return Err(ApiError::invalid("release phases must have the same project, repo and commit"));
    }
    let (size, hash): (u64, String) = c.query_row("SELECT size, sha256 FROM release_attachments WHERE job=?1", [predecessor], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?.ok_or_else(|| ApiError::conflict("attach the smoke result to phase 1 before queueing phase 2"))?;
    c.execute("INSERT INTO release_successors(job, predecessor, attachment_size, attachment_sha256) VALUES (?1,?2,?3,?4)", params![job.id, predecessor, size, hash])?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

fn io(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(format!("release handoff storage: {e}"))
}

pub fn relative_directory(path: &str) -> ApiResult<()> {
    if path.is_empty()
        || Path::new(path).components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ApiError::invalid("release output directory must be a repository-relative path without . or .."));
    }
    Ok(())
}

fn name(name: &str) -> ApiResult<()> {
    if name.is_empty()
        || name.len() > 200
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || matches!(name, "." | ".." | "manifest.json")
    {
        return Err(ApiError::invalid("release artifact names must contain only ASCII letters, digits, dots, underscores and hyphens"));
    }
    Ok(())
}

fn root(state: &Path, job: &str) -> ApiResult<PathBuf> {
    // IDs come from the database, but still never allow path components here.
    name(job)?;
    Ok(state.join("release-handoffs").join(job))
}

fn private_dir(path: &Path) -> ApiResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path).map_err(io)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)
}

/// Validate a complete tar stream into a private staging directory. No archive
/// path is ever used for extraction. Rename publishes the entire set at once.
pub fn publish(state: &Path, job: &str, archive: impl Read, limit: u64) -> ApiResult<Vec<Artifact>> {
    let root = root(state, job)?;
    private_dir(&root)?;
    let stage = tempfile::Builder::new().prefix("publish-").tempdir_in(&root).map_err(io)?;
    let mut artifacts = Vec::new();
    let mut total = 0_u64;
    let mut tar = tar::Archive::new(archive);
    for entry in tar.entries().map_err(io)? {
        let mut entry = entry.map_err(io)?;
        if !entry.header().entry_type().is_file() {
            return Err(ApiError::invalid("release outputs must be regular files, not directories or links"));
        }
        let path = entry.path().map_err(io)?.into_owned();
        let filename = path.to_str().ok_or_else(|| ApiError::invalid("release artifact name is not UTF-8"))?;
        name(filename)?;
        if artifacts.iter().any(|a: &Artifact| a.name == filename) {
            return Err(ApiError::invalid("duplicate release artifact name"));
        }
        let size = entry.size();
        total = total.checked_add(size).ok_or_else(|| ApiError::invalid("release artifact size overflow"))?;
        if total > limit || artifacts.len() >= 1024 {
            return Err(ApiError::invalid(format!("release artifacts exceed the per-job limit of {limit} bytes or 1024 files")));
        }
        let mut file = File::create(stage.path().join(filename)).map_err(io)?;
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 65536];
        let mut copied = 0;
        loop {
            let n = entry.read(&mut buffer).map_err(io)?;
            if n == 0 { break; }
            file.write_all(&buffer[..n]).map_err(io)?;
            hash.update(&buffer[..n]);
            copied += n as u64;
        }
        if copied != size { return Err(ApiError::invalid("truncated release artifact")); }
        file.sync_all().map_err(io)?;
        artifacts.push(Artifact { name: filename.to_owned(), size, sha256: format!("{:x}", hash.finalize()) });
    }
    artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    if root.join("outputs").exists() {
        if self::artifacts(state, job)? == artifacts {
            return Ok(artifacts);
        }
        return Err(ApiError::conflict("release artifacts were already published with different contents"));
    }
    let manifest = serde_json::to_vec(&artifacts).map_err(io)?;
    let mut file = File::create(stage.path().join("manifest.json")).map_err(io)?;
    file.write_all(&manifest).map_err(io)?;
    file.sync_all().map_err(io)?;
    fs::rename(stage.path(), root.join("outputs")).map_err(io)?;
    File::open(&root).and_then(|f| f.sync_all()).map_err(io)?;
    Ok(artifacts)
}

pub fn artifacts(state: &Path, job: &str) -> ApiResult<Vec<Artifact>> {
    let path = root(state, job)?.join("outputs/manifest.json");
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io(e)),
    }
}

pub fn output_path(state: &Path, job: &str, filename: &str) -> ApiResult<PathBuf> {
    name(filename)?;
    if !artifacts(state, job)?.iter().any(|a| a.name == filename) {
        return Err(ApiError::invalid("no such release artifact"));
    }
    Ok(root(state, job)?.join("outputs").join(filename))
}

pub fn attachment(state: &Path, job: &str, bytes: &[u8]) -> ApiResult<Artifact> {
    if bytes.len() > ATTACHMENT_LIMIT {
        return Err(ApiError::invalid("release attachment exceeds 1 MiB"));
    }
    let root = root(state, job)?;
    private_dir(&root)?;
    let mut staged = tempfile::NamedTempFile::new_in(&root).map_err(io)?;
    staged.write_all(bytes).map_err(io)?;
    staged.as_file().sync_all().map_err(io)?;
    // Replacement is intentional. The DB transaction guarding phase startup
    // serializes this operation with linking a successor.
    staged.persist(root.join("attachment")).map_err(io)?;
    Ok(Artifact { name: "attachment".into(), size: bytes.len() as u64, sha256: format!("{:x}", Sha256::digest(bytes)) })
}

pub fn attachment_bytes(state: &Path, job: &str) -> ApiResult<Vec<u8>> {
    fs::read(root(state, job)?.join("attachment")).map_err(io)
}

/// Build a trusted archive, never forwarding the target's original tar paths.
pub fn inputs(state: &Path, link: &Successor) -> ApiResult<Vec<u8>> {
    let mut tar = tar::Builder::new(Vec::new());
    for artifact in artifacts(state, &link.predecessor)? {
        let bytes = fs::read(output_path(state, &link.predecessor, &artifact.name)?).map_err(io)?;
        if bytes.len() as u64 != artifact.size || format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256 {
            return Err(ApiError::invalid("stored release artifact failed SHA-256 verification"));
        }
        append(&mut tar, &format!("artifacts/{}", artifact.name), &bytes)?;
    }
    let bytes = attachment_bytes(state, &link.predecessor)?;
    if bytes.len() as u64 != link.attachment_size || format!("{:x}", Sha256::digest(&bytes)) != link.attachment_sha256 {
        return Err(ApiError::invalid("stored release attachment failed SHA-256 verification"));
    }
    append(&mut tar, "attachment", &bytes)?;
    tar.into_inner().map_err(io)
}

fn append(tar: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) -> ApiResult<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o400);
    header.set_cksum();
    tar.append_data(&mut header, path, bytes).map_err(io)
}

pub fn output_limit() -> ApiResult<u64> {
    match std::env::var("TERRARIUM_RELEASE_ARTIFACT_LIMIT_BYTES") {
        Ok(value) => value.parse::<u64>().ok().filter(|n| *n > 0 && *n <= usize::MAX as u64 - 2 * 1024 * 1024)
            .ok_or_else(|| ApiError::invalid("TERRARIUM_RELEASE_ARTIFACT_LIMIT_BYTES must be a positive bounded byte count")),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_OUTPUT_LIMIT),
        Err(e) => Err(io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            tar.append_data(&mut header, name, *data).unwrap();
        }
        tar.into_inner().unwrap()
    }

    #[test]
    fn publishes_two_files_with_hashes() {
        let state = tempfile::tempdir().unwrap();
        let bytes = archive(&[("signed.apk", b"apk"), ("fixtures.zip", b"fixtures")]);
        let got = publish(state.path(), "job-1", &bytes[..], 100).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got, artifacts(state.path(), "job-1").unwrap());
        assert_eq!(got[1].sha256, format!("{:x}", Sha256::digest(b"apk")));
        assert_eq!(fs::read(output_path(state.path(), "job-1", "signed.apk").unwrap()).unwrap(), b"apk");
    }

    #[test]
    fn oversized_publish_keeps_nothing_partial() {
        let state = tempfile::tempdir().unwrap();
        let bytes = archive(&[("small", b"a"), ("large", b"1234")]);
        assert!(publish(state.path(), "job-1", &bytes[..], 4).is_err());
        assert!(artifacts(state.path(), "job-1").unwrap().is_empty());
        assert_eq!(fs::read_dir(root(state.path(), "job-1").unwrap()).unwrap().count(), 0);
    }

    #[test]
    fn rejects_links_and_nested_names() {
        let state = tempfile::tempdir().unwrap();
        let bytes = archive(&[("nested/file", b"x")]);
        assert!(publish(state.path(), "job-1", &bytes[..], 100).is_err());
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o600);
        header.set_link_name("/etc/passwd").unwrap();
        header.set_cksum();
        tar.append_data(&mut header, "link", &b""[..]).unwrap();
        let bytes = tar.into_inner().unwrap();
        assert!(publish(state.path(), "job-1", &bytes[..], 100).is_err());
    }

    #[test]
    fn attachment_limit_and_digest() {
        let state = tempfile::tempdir().unwrap();
        let got = attachment(state.path(), "job-1", b"smoke").unwrap();
        assert_eq!(got.sha256, format!("{:x}", Sha256::digest(b"smoke")));
        assert!(attachment(state.path(), "job-1", &vec![0; ATTACHMENT_LIMIT + 1]).is_err());
        assert_eq!(attachment_bytes(state.path(), "job-1").unwrap(), b"smoke");
    }
}
