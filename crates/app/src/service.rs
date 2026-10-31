//! Windows service shell around the feeder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use anyhow::{Result, anyhow};
use eosvcam_shared::log;
use windows_services::{Command, Service};

use crate::feeder;

static STOP: AtomicBool = AtomicBool::new(false);

pub fn run() -> Result<()> {
    log::init("feeder", false);
    let mut worker: Option<JoinHandle<()>> = None;
    Service::new()
        .can_stop()
        .run(|_, command| match command {
            Command::Start => {
                STOP.store(false, Ordering::Relaxed);
                worker = Some(thread::spawn(|| {
                    if let Err(e) = feeder::run(&STOP, false) {
                        log!("feeder failed: {e:#}");
                        // Ending without reporting STOPPED counts as a crash, so the service's
                        // restart actions run.
                        std::process::exit(1);
                    }
                }));
            }
            Command::Stop => {
                STOP.store(true, Ordering::Relaxed);
                if let Some(w) = worker.take() {
                    let _ = w.join();
                }
            }
            _ => {}
        })
        .map_err(|e| anyhow!(e))
}
