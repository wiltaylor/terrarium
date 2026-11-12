//! Service configuration is the only mutable executable pointer. Images are
//! immutable version directories, never renamed while selected by SCM.
use crate::update_journal::Journal;
use std::io;
use std::path::{Path, PathBuf};
use windows_service::{
    service::{ServiceAccess, ServiceInfo},
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const NAME: &str = "TerrariumHost";

pub fn journal_path() -> PathBuf {
    crate::config::data_dir().join("host").join("update.json")
}

fn service() -> io::Result<windows_service::service::Service> {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|manager| {
            manager.open_service(
                NAME,
                ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG,
            )
        })
        .map_err(io::Error::other)
}

pub fn configured_image() -> io::Result<PathBuf> {
    let command = service()?
        .query_config()
        .map_err(io::Error::other)?
        .executable_path;
    let command = command
        .to_str()
        .ok_or_else(|| io::Error::other("non-Unicode service command"))?;
    let image = command
        .strip_prefix('"')
        .and_then(|s| s.split_once('"').map(|p| p.0))
        .ok_or_else(|| io::Error::other("service ImagePath must quote its executable"))?;
    Ok(PathBuf::from(image))
}

pub fn switch(image: &Path) -> io::Result<()> {
    if !image.is_file() {
        return Err(io::Error::other("service image does not exist"));
    }
    let service = service()?;
    let config = service.query_config().map_err(io::Error::other)?;
    // Retain this service process's config arguments without reading the file.
    service
        .change_config(&ServiceInfo {
            name: NAME.into(),
            display_name: config.display_name,
            service_type: config.service_type,
            start_type: config.start_type,
            error_control: config.error_control,
            executable_path: image.to_owned(),
            launch_arguments: std::env::args_os().skip(1).collect(),
            dependencies: config.dependencies,
            account_name: None,
            account_password: None,
        })
        .map_err(io::Error::other)
}

pub fn activate(binary: &Path, version: &str) -> io::Result<()> {
    let root = binary
        .parent()
        .and_then(Path::parent)
        .filter(|p| p.file_name().is_some_and(|name| name == "versions"))
        .ok_or_else(|| {
            io::Error::other("self-update requires a versioned Program Files installation")
        })?;
    let pending = journal_path();
    if pending.exists() {
        return Err(io::Error::other("an update transaction is already pending"));
    }
    if configured_image()? != binary {
        return Err(io::Error::other(
            "running image differs from SCM configuration",
        ));
    }
    let transaction = Journal::stage(
        &crate::selfupdate::sidecar(binary, crate::selfupdate::NEW),
        binary,
        root,
        version,
    )?;
    transaction.activate(&pending, switch)
}

pub fn confirm(binary: &Path) -> Option<String> {
    let path = journal_path();
    let mut transaction = Journal::read(&path).ok()?;
    if transaction.confirmed
        || transaction.next != binary
        || transaction.version != terrarium_version::FULL
        || configured_image().ok()?.as_path() != binary
    {
        return None;
    }
    transaction.confirmed = true;
    transaction.persist(&path).ok()?;
    Some(transaction.version)
}

pub fn rollback(binary: &Path) -> io::Result<()> {
    let transaction = Journal::read(&journal_path())?;
    if transaction.next != binary || transaction.confirmed {
        return Ok(());
    }
    transaction.rollback(switch)
}

/// A later start, not the welcome callback, may collect the old image. A
/// journal remaining after a rollback is resolved by SCM's configured path.
pub fn startup(binary: &Path) -> io::Result<bool> {
    let path = journal_path();
    let transaction = match Journal::read(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if configured_image()? != binary {
        return Err(io::Error::other("SCM image changed during startup"));
    }
    if binary == transaction.previous {
        std::fs::write(
            crate::selfupdate::sidecar(binary, crate::selfupdate::FAILED),
            transaction.version,
        )?;
        std::fs::remove_file(path)?;
        return Ok(false);
    }
    if binary != transaction.next {
        return Err(io::Error::other(
            "update journal does not match running image",
        ));
    }
    if transaction.confirmed {
        // Do not follow a journal to an unrelated directory when collecting.
        if transaction.previous.parent().and_then(Path::parent)
            == binary.parent().and_then(Path::parent)
            && transaction.previous != binary
            && let Some(directory) = transaction.previous.parent()
        {
            std::fs::remove_dir_all(directory)?;
        }
        std::fs::remove_file(path)?;
        Ok(false)
    } else {
        Ok(true)
    }
}

pub fn checking() -> bool {
    Journal::read(&journal_path()).is_ok_and(|journal| !journal.confirmed)
}
