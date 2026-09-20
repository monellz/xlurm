#![cfg(target_os = "linux")]

mod auth;
mod cli;
mod daemon;
mod device;
pub mod executor;
pub mod model;
mod scheduler;
pub mod storage;

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

fn install_signals() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    Ok(())
}

pub fn entry(command: Option<&str>) {
    // Job environments and output may contain secrets; spool files are private.
    unsafe {
        libc::umask(0o077);
    }
    let code = match cli::run(command) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("xlurm: {error:#}");
            1
        }
    };
    std::process::exit(code);
}
