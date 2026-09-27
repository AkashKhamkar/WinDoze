// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("WinDoze is Windows-only. Build with --target x86_64-pc-windows-gnu (or -msvc).");

mod audio;
mod config;
mod engine;
mod freezer;
mod log;
mod shared;
mod ui;
mod uia;
mod watchdog;
mod win;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW};
use windows::core::w;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let pid_after = |flag: &str| {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).and_then(|s| s.parse::<u32>().ok())
    };
    if let Some(pid) = pid_after("--watchdog-launcher") {
        log::init();
        watchdog::launch(pid);
        return;
    }
    if let Some(pid) = pid_after("--watchdog") {
        log::init();
        watchdog::run(pid);
        return;
    }

    log::init();
    if !claim_single_instance() {
        unsafe {
            MessageBoxW(
                None,
                w!("WinDoze is already running. Look for the moon icon in the system tray."),
                w!("WinDoze"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        return;
    }
    logln!("WinDoze {} starting", env!("CARGO_PKG_VERSION"));

    // Anything a crashed/killed previous run left frozen gets resumed first.
    freezer::recover_from_journal();
    install_panic_hook();
    watchdog::spawn();

    shared::shared().config = config::load();
    engine::start();

    let start_hidden = args.iter().any(|a| a == "--minimized");
    if let Err(e) = ui::run(start_hidden) {
        logln!("UI error: {e}");
        // Most likely no OpenGL 2+ (some VMs / Remote Desktop). Don't just vanish.
        let log_path = config::data_dir().join("windoze.log");
        let text = format!(
            "WinDoze couldn't open its window:\n\n{e}\n\nThis usually means the graphics driver has no OpenGL 2 support (common in virtual machines and Remote Desktop).\n\nLog: {}",
            log_path.display()
        );
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            MessageBoxW(None, windows::core::PCWSTR(wide.as_ptr()), w!("WinDoze"), MB_OK | MB_ICONERROR);
        }
    }
    ui::quit();
}

fn claim_single_instance() -> bool {
    unsafe {
        match CreateMutexW(None, true, w!("Local\\WinDoze.SingleInstance")) {
            Ok(handle) => {
                let already = GetLastError() == ERROR_ALREADY_EXISTS;
                let _ = handle; // deliberately never closed: held for the life of the process
                !already
            }
            Err(_) => true,
        }
    }
}

/// Any panic on any thread: thaw everything, log, and exit.
/// A half-working WinDoze that can't thaw is worse than none.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        logln!("PANIC: {info}");
        freezer::emergency_thaw();
        default(info);
        std::process::exit(101);
    }));
}
