mod convert;
mod decode;
mod edsdk;
mod feeder;
mod install;
mod sdk;
mod selftest;
mod service;

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use eosvcam_shared::log;
use eosvcam_shared::windows_bindings::SetConsoleCtrlHandler;
use windows_core::BOOL;

/// Release builds set EOSVCAM_VERSION from the release tag; local builds report "dev".
const VERSION: &str = match option_env!("EOSVCAM_VERSION") {
    Some(v) => v,
    None => "dev",
};

const USAGE: &str = "usage: eosvcam <command>
  install [--sdk PATH]     install the virtual camera and feeder service (run as administrator)
  uninstall                remove them
  status                   show what is installed and what the feeder is doing
  console [--synthetic]    run the feeder in this window (Ctrl+C stops); --synthetic needs no camera
  demand [seconds]         ask the running feeder for frames and print the rate (tests the camera path)
  selftest [seconds]       check the media source against a synthetic feeder, no camera needed
  version                  print the version
  service                  entry point used by the Windows service manager";

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl_c(_: u32) -> BOOL {
    STOP.store(true, Ordering::Relaxed);
    true.into()
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("service") => service::run(),
        Some("console") => {
            log::init("feeder", true);
            // SAFETY: the handler only touches an atomic.
            unsafe { SetConsoleCtrlHandler(Some(on_ctrl_c), true).ok()? };
            feeder::run(&STOP, args.iter().any(|a| a == "--synthetic"))
        }
        Some("install") => {
            let sdk = match (args.get(1).map(String::as_str), args.get(2)) {
                (Some("--sdk"), Some(p)) => Some(std::path::PathBuf::from(p)),
                (None, _) => None,
                _ => bail!("{USAGE}"),
            };
            install::install(sdk.as_deref())
        }
        Some("uninstall") => install::uninstall(),
        Some("status") => install::status(),
        Some("version" | "--version") => {
            println!("eosvcam {VERSION}");
            Ok(())
        }
        Some("demand") => selftest::demand(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10)),
        Some("selftest") => selftest::run(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(6.0)),
        _ => bail!("{USAGE}"),
    }
}
