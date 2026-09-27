//! State shared between the UI thread and the engine thread.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use crate::config::Config;

#[derive(Clone, Debug)]
pub enum RuleState {
    NotRunning,
    Disabled,
    Paused,
    /// Condition not met; the string says why (e.g. "In use", "Playing audio").
    Waiting(String),
    /// Condition met; will freeze when the countdown reaches zero.
    Counting(Duration),
    /// `trimmed`: its memory has been pushed out of RAM (only done when RAM is low).
    Frozen { for_secs: u64, saved_bytes: u64, trimmed: bool },
    /// Freeze failed; retrying later.
    Error(String),
}

#[derive(Clone, Debug)]
pub struct RuleStatus {
    pub state: RuleState,
    pub procs: usize,
    pub mem_bytes: u64,
    pub cpu_percent: f32,
}

#[derive(Clone, Debug)]
pub struct AppInfo {
    pub exe: String,
    pub name: String,
    pub title: String,
    pub procs: usize,
    pub mem_bytes: u64,
}

#[derive(Default, Clone)]
pub struct Status {
    pub apps: Vec<AppInfo>,
    pub rules: HashMap<String, RuleStatus>,
    pub total_saved_bytes: u64,
    pub frozen_count: usize,
    /// Physical RAM, and how much of it Windows says is available right now.
    pub system_total: u64,
    pub system_available: u64,
    pub memory_low: bool,
}

pub enum Command {
    FreezeNow(String),
    Thaw(String),
    ThawAll,
    /// Force quit every process of the app.
    Kill(String),
}

#[derive(Default)]
pub struct Shared {
    pub config: Config,
    pub status: Status,
    pub commands: Vec<Command>,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| Mutex::new(Shared::default()));

pub fn shared() -> MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Queue a command for the engine and wake it immediately.
pub fn send(cmd: Command) {
    shared().commands.push(cmd);
    crate::engine::wake();
}

/// The UI's egui context, so background threads (tray, engine, hotkey) can ask it to repaint.
pub static UI_CTX: OnceLock<eframe::egui::Context> = OnceLock::new();

pub fn repaint_ui() {
    if let Some(ctx) = UI_CTX.get() {
        ctx.request_repaint();
    }
}

/// Toggle auto-freeze. Pausing thaws everything.
pub fn set_paused(paused: bool) {
    let cfg = {
        let mut s = shared();
        s.config.paused = paused;
        s.config.clone()
    };
    crate::config::save(&cfg);
    if paused {
        crate::freezer::thaw_all("paused");
    }
    crate::logln!("auto-doze {}", if paused { "paused" } else { "resumed" });
    crate::engine::wake();
    repaint_ui();
}
