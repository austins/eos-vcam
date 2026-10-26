//! Windows service shell around the feeder.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use anyhow::Result;
use eosvcam_shared::{SERVICE_NAME, log};
use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING,
    SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    StartServiceCtrlDispatcherW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::feeder;

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);

pub fn run() -> Result<()> {
    let mut name: Vec<u16> = SERVICE_NAME.encode_utf16().chain([0]).collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: the table is terminated by a null entry and outlives the dispatcher call.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr())? };
    Ok(())
}

fn report(state: SERVICE_STATUS_CURRENT_STATE) {
    report_exit(state, NO_ERROR.0);
}

/// A non-zero `exit_code` on STOPPED counts as a failure, so the service's restart actions run.
fn report_exit(state: SERVICE_STATUS_CURRENT_STATE, exit_code: u32) {
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
    } else {
        0
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_STOPPED { 0 } else { 10_000 },
    };
    let handle = SERVICE_STATUS_HANDLE(STATUS_HANDLE.load(Ordering::Relaxed) as *mut c_void);
    // SAFETY: handle came from RegisterServiceCtrlHandlerExW and status is a live local.
    unsafe {
        let _ = SetServiceStatus(handle, &status);
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    log::init("feeder", false);
    let name = HSTRING::from(SERVICE_NAME);
    // SAFETY: name is valid for the call and the handler is a plain function.
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control_handler), None)
    };
    let Ok(handle) = handle else {
        log!("could not register the service control handler");
        return;
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::Relaxed);
    report(SERVICE_START_PENDING);
    report(SERVICE_RUNNING);
    let failed = feeder::run(&STOP, false)
        .map_err(|e| log!("feeder failed: {e:#}"))
        .is_err();
    report_exit(SERVICE_STOPPED, if failed { 1 } else { NO_ERROR.0 });
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _ctx: *mut c_void,
) -> u32 {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        STOP.store(true, Ordering::Relaxed);
        report(SERVICE_STOP_PENDING);
    }
    NO_ERROR.0
}
