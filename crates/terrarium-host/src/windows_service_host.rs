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
