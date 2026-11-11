//! `/api/backups`: make, list, and download backups (docs/api.md §32).
//!
//! The operator only: a backup holds every secret of the daemon. Each
//! create and each download writes an audit row. A restore is offline
//! only (`terrariumd backup restore`), because it replaces the database
//! under the daemon.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json};
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;

use super::Shared;
use crate::actor::Actor;
use crate::backup::{self, CreateOptions, Created, DbSource, Kind, Roots};
use crate::error::{ApiError, ApiResult};
use crate::policy;

/// One backup at a time per backup dir, from the API or the schedule.
/// Per dir, not one for the process: the tests run many daemons in one.
static BUSY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(Default::default);

fn busy_lock(dir: &std::path::Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut m = BUSY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    m.entry(dir.to_path_buf()).or_default().clone()
}

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route("/backups", get(list).post(create), &[(axum::http::Method::GET, super::link_registry::Access::OperatorOnly), (axum::http::Method::POST, super::link_registry::Access::OperatorOnly)])
        .route("/backups/{name}", get(download), &[(axum::http::Method::GET, super::link_registry::Access::OperatorOnly)])
}

/// The body of `POST /api/backups`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    /// `full` (the default) or `diff`.
    #[serde(default)]
    pub kind: Option<Kind>,
    /// Add the agent workspaces (the work root).
    #[serde(default)]
    pub work: bool,
    /// Seal the backup with this passphrase.
    #[serde(default)]
    pub passphrase: Option<String>,
    /// The base of a diff, a file name in the backup dir. Default: the
    /// newest full backup.
    #[serde(default)]
    pub base: Option<String>,
}

/// The backup dir of the daemon.
pub fn dir(state: &Shared) -> PathBuf {
    backup::default_dir(&state.state_dir)
}

/// Make a backup while the daemon runs. The database copy is a
/// `VACUUM INTO` on the daemon's own connection (one consistent
/// snapshot); the files are read as they are. `actor` labels the audit row.
pub async fn create_online(state: &Shared, body: CreateBody, actor: &str) -> ApiResult<Created> {
    let lock = busy_lock(&dir(state));
    let Ok(_busy) = lock.try_lock() else {
        return Err(ApiError::conflict(
            "a backup runs now; try again when it ends",
        ));
    };
    create_locked(state, body, actor).await
}

/// The one-backup lock of the backup dir, waited for when a backup runs
/// now: for the schedule. It holds the guard from the choice of the kind
/// and the base to the prune, so no manual backup comes in between.
pub async fn lock_queued(state: &Shared) -> tokio::sync::OwnedMutexGuard<()> {
    busy_lock(&dir(state)).lock_owned().await
}

/// [`create_online`] under the guard of [`lock_queued`].
pub async fn create_held(
    state: &Shared,
    _held: &tokio::sync::OwnedMutexGuard<()>,
    body: CreateBody,
    actor: &str,
) -> ApiResult<Created> {
    create_locked(state, body, actor).await
}

async fn create_locked(state: &Shared, body: CreateBody, actor: &str) -> ApiResult<Created> {
    let dir = dir(state);
    let kind = body.kind.unwrap_or(Kind::Full);
    let base = match body.base.as_deref() {
        None => None,
        Some(name) if backup::is_backup_name(name) => Some(dir.join(name)),
        Some(name) => return Err(ApiError::invalid(format!("bad backup name `{name}`"))),
    };
    if body.passphrase.as_deref() == Some("") {
        return Err(ApiError::invalid("the passphrase is empty"));
    }
    let tmp = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            tempfile::Builder::new().prefix(".backup-").tempdir_in(&dir)
        })
        .await
        .map_err(|e| ApiError::internal(format!("backup task failed: {e}")))?
        .map_err(|e| ApiError::internal(format!("cannot make the backup dir: {e}")))?
    };
    let snapshot = tmp.path().join(backup::DB_FILE);
    {
        let snapshot = snapshot.clone();
        state
            .db
            .read(move |c| {
                backup::vacuum_into(c, &snapshot)
                    .map_err(|e| ApiError::internal(format!("cannot copy the database: {e}")))
            })
            .await?;
    }
    let roots = Roots {
        state: state.state_dir.clone(),
        config: state.config_repo.get().ok().map(|r| r.dir),
        work: body.work.then(|| state.work_dir.clone()),
    };
    let opts = CreateOptions {
        kind,
        dir,
        base,
        passphrase: body.passphrase,
        db: DbSource::Snapshot(snapshot),
    };
    let created = tokio::task::spawn_blocking(move || {
        let mut opts = opts;
        let r = backup::create(&roots, &opts);
        if let Some(p) = opts.passphrase.as_mut() {
            zeroize::Zeroize::zeroize(p);
        }
        drop(tmp);
        r
    })
    .await
    .map_err(|e| ApiError::internal(format!("backup task failed: {e}")))?
    .map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    let detail = json!({
        "name": created.name,
        "id": created.header.id,
        "kind": created.header.kind,
        "base": created.header.base.as_ref().map(|b| b.id.clone()),
        "roots": created.header.roots,
        "encrypted": created.header.encryption.is_some(),
        "entries": created.header.entries,
        "stored_files": created.header.stored_files,
        "size": created.size,
    });
    let name = created.name.clone();
    let r = state
        .db
        .write(actor.to_owned(), move |t| {
            t.audit("backup.create", &name, None, detail)
        })
        .await;
    if let Err(e) = r {
        tracing::warn!("cannot write the audit row of backup.create: {}", e.message);
    }
    tracing::info!(name = %created.name, kind = %created.header.kind, size = created.size, "backup made");
    Ok(created)
}

/// `POST /api/backups`: make a backup now.
async fn create(
    State(state): State<Shared>,
    actor: Actor,
    raw: Bytes,
) -> ApiResult<(StatusCode, Json<Created>)> {
    policy::operator_only(&actor, "making a backup")?;
    // No body: a full backup with the defaults.
    let body: CreateBody = if raw.iter().all(u8::is_ascii_whitespace) {
        CreateBody::default()
    } else {
        serde_json::from_slice(&raw).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let created = create_online(&state, body, &actor.label()).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /api/backups`: the backups in the backup dir, oldest first.
async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<backup::Listed>>> {
    policy::operator_only(&actor, "listing the backups")?;
    let dir = dir(&state);
    let items = tokio::task::spawn_blocking(move || backup::list(&dir))
        .await
        .map_err(|e| ApiError::internal(format!("backup task failed: {e}")))?
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(items))
}

/// `GET /api/backups/{name}`: the zip file.
async fn download(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    policy::operator_only(&actor, "downloading a backup")?;
    if !backup::is_backup_name(&name) {
        return Err(ApiError::not_found(format!("no backup `{name}`")));
    }
    let path = dir(&state).join(&name);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::not_found(format!("no backup `{name}`")))?;
    let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let audit_name = name.clone();
    let r = state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "backup.download",
                &audit_name,
                None,
                json!({"name": audit_name, "size": size}),
            )
        })
        .await;
    if let Err(e) = r {
        tracing::warn!(
            "cannot write the audit row of backup.download: {}",
            e.message
        );
    }
    let stream = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buf = vec![0u8; 64 * 1024];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    let mut resp = Body::from_stream(stream).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}
