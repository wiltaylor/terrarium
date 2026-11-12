//! SCM lifecycle and bounded health check for self-updates. No helper process.
use std::ffi::OsString;
use std::time::Duration;
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

pub const NAME: &str = "TerrariumHost";
define_windows_service!(ffi_main, service_main);

pub fn dispatch() -> anyhow::Result<()> {
    service_dispatcher::start(NAME, ffi_main)?;
    Ok(())
}

fn status(state: ServiceState, code: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn service_main(_: Vec<OsString>) {
    if let Err(error) = serve() {
        tracing::error!(%error, "Windows service failed");
    }
}

fn serve() -> anyhow::Result<()> {
    use terrarium_host::windows_update;
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
    let binary = std::env::current_exe()?;
    let pending = windows_update::journal_path();
    let checking = windows_update::startup(&binary)?;
    let startup_pending = std::fs::read(&pending).ok();
    let mut operator_stop = false;
    let result = (|| -> anyhow::Result<()> {
        use clap::Parser;
        let cfg = super::load_config(&super::Args::parse())?;
        let rt = tokio::runtime::Runtime::new()?;
        handle.set_service_status(status(ServiceState::Running, 0))?;
        let result = rt.block_on(async {
            tokio::select! {
                result = super::run(cfg) => result,
                _ = stopped.recv() => { operator_stop = true; Ok(()) },
                _ = async {
                    if !checking { std::future::pending::<()>().await; }
                    // A welcome removes the journal only for this build.
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
                    while windows_update::checking() {
                        if tokio::time::Instant::now() >= deadline { return; }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    std::future::pending::<()>().await;
                } => Err(anyhow::anyhow!("updated service did not reach the daemon")),
            }
        });
        handle.set_service_status(status(ServiceState::StopPending, 0))?;
        rt.shutdown_timeout(Duration::from_secs(3));
        result
    })();
    // A later successful update in this process has a different journal.
    // Never mistake it for the startup transaction's failed health check.
    if checking && std::fs::read(&pending).ok() == startup_pending && !operator_stop {
        windows_update::rollback(&binary)?;
    }
    // A deliberate operator stop never invokes failure recovery. An update,
    // rollback or unexpected failure does, including a normal dial-loop exit.
    let restart = !operator_stop && (pending.exists() || checking || result.is_err());
    handle.set_service_status(status(ServiceState::Stopped, u32::from(restart)))?;
    result
}
