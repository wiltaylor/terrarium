//! Service Control Manager lifecycle. Session processes remain in their own jobs.

use std::ffi::OsString;
use std::time::Duration;

use windows_service::{
    define_windows_service,
    service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType},
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

pub const NAME: &str = "TerrariumHost";

define_windows_service!(ffi_main, service_main);

pub fn dispatch() -> anyhow::Result<()> {
    recover_pending()?;
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(10));
        if let Err(error) = recover_pending() {
            tracing::error!(%error, "cannot resume interrupted service update");
        }
    });
    service_dispatcher::start(NAME, ffi_main)?;
    Ok(())
}

fn status(state: ServiceState, code: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else { ServiceControlAccept::empty() },
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn service_main(_: Vec<OsString>) {
    if let Err(e) = serve() {
        tracing::error!(error = %e, "Windows service failed");
    }
}

fn serve() -> anyhow::Result<()> {
    let (shutdown, mut stopped) = tokio::sync::mpsc::channel(1);
    let handle = service_control_handler::register(NAME, move |event| match event {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = shutdown.try_send(());
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    handle.set_service_status(status(ServiceState::StartPending, 0))?;
    let result = (|| -> anyhow::Result<()> {
        use clap::Parser;
        let args = super::Args::parse();
        let cfg = super::load_config(&args)?;
        let rt = tokio::runtime::Runtime::new()?;
        handle.set_service_status(status(ServiceState::Running, 0))?;
        let result = rt.block_on(async {
            tokio::select! {
                result = super::run(cfg) => result,
                _ = stopped.recv() => Ok(()),
            }
        });
        handle.set_service_status(status(ServiceState::StopPending, 0))?;
        rt.shutdown_timeout(Duration::from_secs(3));
        result
    })();
    handle.set_service_status(status(ServiceState::Stopped, if result.is_ok() { 0 } else { 1 }))?;
    result
}

/// Stop, replace, restart, and roll back unless the new build receives a
/// daemon welcome. Runs outside the service from a separate verified copy.
pub fn apply_update(binary: &std::path::Path, version: &str, resume: bool) -> anyhow::Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use terrarium_host::selfupdate::{self, sidecar};

    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(sidecar(binary, "update.lock"))?;
    // Held until this helper exits. A restart can distinguish an active
    // transaction from one interrupted by a helper crash or a reboot.
    if lock.try_lock().is_err() { return Ok(()); }
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(NAME, ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
    let wait_stopped = || -> anyhow::Result<()> {
        let until = std::time::Instant::now() + Duration::from_secs(30);
        while service.query_status()?.current_state != ServiceState::Stopped {
            anyhow::ensure!(std::time::Instant::now() < until, "service did not stop for update");
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    };
    let previous = sidecar(binary, selfupdate::PREV);
    if !resume {
        // Validate before stopping a working service. Never move its pathname
        // away: copy a flushed backup, then atomically replace in place.
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(selfupdate::smoke_test(&sidecar(binary, selfupdate::NEW), version, Duration::from_secs(10)))?;
        rt.shutdown_background();
        if service.query_status()?.current_state != ServiceState::Stopped { service.stop()?; }
        wait_stopped()?;
    }
    let pending = sidecar(binary, selfupdate::PENDING);
    let install = (|| -> anyhow::Result<()> {
        if !resume {
            selfupdate::windows_prepare_replacement(binary, version)?;
            selfupdate::windows_replace(&sidecar(binary, selfupdate::NEW), binary)?;
            service.start::<&str>(&[])?;
        }
        let until = std::time::Instant::now() + Duration::from_secs(120);
        while pending.exists() {
            anyhow::ensure!(std::time::Instant::now() < until, "updated service did not reach the daemon");
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    })();
    if install.is_err() {
        if service.query_status()?.current_state != ServiceState::Stopped { let _ = service.stop(); }
        wait_stopped()?;
        let rollback = sidecar(binary, "rollback.tmp");
        std::fs::copy(&previous, &rollback)?;
        std::fs::File::open(&rollback)?.sync_all()?;
        selfupdate::windows_replace(&rollback, binary)?;
        std::fs::write(sidecar(binary, selfupdate::FAILED), format!("{version}\n"))?;
        let _ = std::fs::remove_file(pending);
        service.start::<&str>(&[])?;
    }
    install
}

/// On service restart, an orphaned pending transaction gets its own recovery
/// helper. An active helper holds the lock and remains responsible for it.
fn recover_pending() -> anyhow::Result<()> {
    use terrarium_host::selfupdate::{self, sidecar};
    let binary = std::env::current_exe()?;
    let pending = sidecar(&binary, selfupdate::PENDING);
    let Ok(version) = std::fs::read_to_string(pending) else { return Ok(()); };
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(sidecar(&binary, "update.lock"))?;
    if lock.try_lock().is_err() { return Ok(()); }
    drop(lock);
    std::process::Command::new(sidecar(&binary, "update-helper.exe"))
        .arg("resume-service-update").arg(&binary).arg(version.trim())
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).spawn()?;
    Ok(())
}
