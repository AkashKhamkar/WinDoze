//! A tiny second process that waits for the main WinDoze process to exit
//! (for any reason, including "End task" in Task Manager) and then resumes
//! anything the journal says is still frozen.
//!
//! It's started through a short-lived launcher process so it isn't a direct
//! child of WinDoze; otherwise Task Manager groups it under WinDoze and
//! "End task" would kill the watchdog along with the thing it's watching.

use std::os::windows::process::CommandExt;

use windows::Win32::System::Threading::{INFINITE, PROCESS_SYNCHRONIZE, WaitForSingleObject};

use crate::logln;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

fn spawn_self(flag: &str, parent_pid: u32) -> std::io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .args([flag, &parent_pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
}

/// Called by the main process.
pub fn spawn() {
    if let Err(e) = spawn_self("--watchdog-launcher", std::process::id()) {
        logln!("WARNING: could not start watchdog: {e}");
    }
}

/// Runs in the short-lived launcher: start the real watchdog, then exit so it's orphaned.
pub fn launch(parent_pid: u32) {
    match spawn_self("--watchdog", parent_pid) {
        Ok(child) => logln!("watchdog started (pid {})", child.id()),
        Err(e) => logln!("WARNING: could not start watchdog: {e}"),
    }
}

pub fn run(parent_pid: u32) {
    let Ok(parent) = crate::win::open_process(parent_pid, PROCESS_SYNCHRONIZE) else {
        // Parent already gone: recover right away.
        crate::freezer::recover_from_journal();
        return;
    };
    unsafe { WaitForSingleObject(parent.0, INFINITE) };
    crate::freezer::recover_from_journal();
    logln!("watchdog: WinDoze (pid {parent_pid}) exited; journal checked");
}
