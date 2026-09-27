//! Tiny append-only file logger. WinDoze has no console, so this file is the
//! only way to see what it did: %APPDATA%\WinDoze\windoze.log

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

static LOG: Mutex<Option<File>> = Mutex::new(None);
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

pub fn init() {
    let path = crate::config::data_dir().join("windoze.log");
    if std::fs::metadata(&path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
    }
}

pub fn write(msg: &str) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} [{}] {}\n",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        std::process::id(),
        msg
    );
    // try_lock so a panic while logging can never deadlock the panic hook.
    let mut guard = match LOG.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    if let Some(file) = guard.as_mut() {
        let _ = file.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
