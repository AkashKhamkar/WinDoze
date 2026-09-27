//! The engine thread: watches focus/minimize/CPU/audio and decides when to
//! freeze and thaw. It owns a hidden top-level window so it gets a message
//! loop (needed for the foreground hook), a timer, the panic hotkey, and
//! WM_QUERYENDSESSION (so nothing stays frozen across logoff/shutdown).
//! None of this depends on the UI window being visible.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GetMessageW, MSG, PostMessageW,
    RegisterClassW, SetTimer, TranslateMessage, WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    WM_APP, WM_ENDSESSION, WM_HOTKEY, WM_QUERYENDSESSION, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::w;

use crate::config::{Config, Mode, Rule};
use crate::freezer;
use crate::logln;
use crate::shared::{self, AppInfo, Command, RuleState, RuleStatus, Status};
use crate::win::{self, AppWindow, ProcEntry};

const WM_WAKE: u32 = WM_APP + 1;
const TICK_MS: u32 = 1000;
const HOTKEY_ID: i32 = 1;
/// "0 minutes" still waits this long, so a quick Alt-Tab never freezes anything.
const GRACE: Duration = Duration::from_secs(5);
/// Treat an app as "playing audio" for this long after its last sound.
const AUDIO_HOLD: Duration = Duration::from_secs(30);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(60);
const APPS_SCAN_EVERY: Duration = Duration::from_secs(3);

static ENGINE_HWND: AtomicIsize = AtomicIsize::new(0);
/// Set between WM_QUERYENDSESSION and a cancelled WM_ENDSESSION: never freeze
/// while Windows is signing out, or apps would block shutdown.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

/// Ask the engine to run a tick now (process commands, refresh status).
pub fn wake() {
    let h = ENGINE_HWND.load(Ordering::Acquire);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn start() {
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(run)
        .expect("failed to start engine thread");
}

fn run() {
    unsafe {
        // MTA: COM calls (audio) must not pump messages and re-enter the engine.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("WinDozeEngine");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinstance.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        // A hidden *top-level* window (not message-only) so it receives WM_QUERYENDSESSION.
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("WinDoze engine"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                logln!("FATAL: could not create engine window: {e}");
                return;
            }
        };
        ENGINE_HWND.store(hwnd.0 as isize, Ordering::Release);

        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if hook.is_invalid() {
            logln!("WARNING: foreground hook failed; relying on the 1s poll to wake apps");
        }
        if let Err(e) = RegisterHotKey(Some(hwnd), HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, 'T' as u32) {
            logln!("WARNING: could not register Ctrl+Alt+Shift+T ({e}); use the tray menu to wake apps");
        }
        SetTimer(Some(hwnd), 1, TICK_MS, None);

        ENGINE.with(|e| *e.borrow_mut() = Some(Engine::new()));
        logln!("engine started");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn with_engine(f: impl FnOnce(&mut Engine)) {
    ENGINE.with(|cell| {
        // try_borrow_mut: never re-enter the engine if a callback arrives mid-tick.
        if let Ok(mut guard) = cell.try_borrow_mut()
            && let Some(engine) = guard.as_mut() {
                f(engine);
            }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER | WM_WAKE => {
            with_engine(|e| e.tick());
            LRESULT(0)
        }
        WM_HOTKEY => {
            logln!("panic hotkey pressed: waking everything and pausing");
            shared::set_paused(true);
            LRESULT(0)
        }
        WM_QUERYENDSESSION => {
            SHUTTING_DOWN.store(true, Ordering::SeqCst);
            freezer::thaw_all("Windows is signing out / shutting down");
            with_engine(|e| e.reset_all());
            LRESULT(1)
        }
        WM_ENDSESSION => {
            if wparam.0 == 0 {
                // Sign-out/shutdown was cancelled: go back to normal.
                SHUTTING_DOWN.store(false, Ordering::SeqCst);
                logln!("sign-out/shutdown cancelled; auto-doze resumes");
            } else {
                freezer::thaw_all("session ending");
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND {
        return;
    }
    // Thaw path #1: the user switched to a frozen app (taskbar, Alt-Tab, click).
    // pid_for_window maps Windows' "ghost" stand-in window back to the frozen app.
    if let Some(pid) = win::pid_for_window(hwnd)
        && let Some(exe) = freezer::frozen_exe_for_pid(pid) {
            freezer::thaw(&exe, "switched to it");
            with_engine(|e| e.reset_rule(&exe));
            wake();
            shared::repaint_ui();
        }
}

// ---------------------------------------------------------------------------

#[derive(Default)]
struct RuleRuntime {
    /// When the rule's condition started holding continuously.
    cond_since: Option<Instant>,
    last_audio: Option<Instant>,
    cpu_sample: Option<(Instant, u64)>,
    cpu_percent: Option<f32>,
    retry_after: Option<Instant>,
    last_error: Option<String>,
}

struct Engine {
    rt: HashMap<String, RuleRuntime>,
    ncpu: f64,
    last_apps_scan: Option<Instant>,
    apps_cache: Vec<AppInfo>,
}

impl Engine {
    fn new() -> Self {
        Engine {
            rt: HashMap::new(),
            ncpu: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64,
            last_apps_scan: None,
            apps_cache: Vec::new(),
        }
    }

    fn reset_rule(&mut self, exe: &str) {
        if let Some(rt) = self.rt.get_mut(exe) {
            rt.cond_since = None;
        }
    }

    fn reset_all(&mut self) {
        self.rt.values_mut().for_each(|rt| rt.cond_since = None);
    }

    fn tick(&mut self) {
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            self.reset_all();
            return;
        }
        let now = Instant::now();
        let (cfg, commands) = {
            let mut s = shared::shared();
            (s.config.clone(), std::mem::take(&mut s.commands))
        };

        let procs = win::snapshot_processes();
        let windows = win::app_windows();
        let tree = ProcTree::new(&procs);
        let rule_exes: HashSet<String> = cfg.rules.iter().map(|r| r.exe.clone()).collect();
        let excluded: HashSet<String> =
            cfg.excluded_children.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
        let group_of = |exe: &str| tree.group(exe, &windows, &excluded, &rule_exes);

        for cmd in commands {
            match cmd {
                Command::ThawAll => {
                    freezer::thaw_all("wake all");
                    self.reset_all();
                }
                Command::Thaw(exe) => {
                    freezer::thaw(&exe, "woken from WinDoze");
                    self.reset_rule(&exe);
                }
                Command::FreezeNow(exe) => {
                    let trim = cfg.rule(&exe).map(|r| r.trim).unwrap_or(true);
                    let rt = self.rt.entry(exe.clone()).or_default();
                    match freezer::freeze_group(&exe, &group_of(&exe), trim) {
                        Ok(_) => rt.last_error = None,
                        Err(e) => {
                            logln!("manual doze of {exe} failed: {e}");
                            rt.last_error = Some(e);
                            rt.retry_after = Some(now + RETRY_AFTER_ERROR);
                        }
                    }
                }
            }
        }

        if cfg.paused && !freezer::frozen_info().is_empty() {
            freezer::thaw_all("paused");
        }

        let fg = win::foreground_pid();
        let frozen: HashMap<String, freezer::FrozenInfo> =
            freezer::frozen_info().into_iter().map(|f| (f.exe.clone(), f)).collect();
        let groups: HashMap<String, Vec<u32>> = cfg.rules.iter().map(|r| (r.exe.clone(), group_of(&r.exe))).collect();

        let need_audio = cfg.skip_if_playing_audio
            && !cfg.paused
            && cfg.rules.iter().any(|r| r.enabled && !frozen.contains_key(&r.exe) && !groups[&r.exe].is_empty());
        let audio = if need_audio { crate::audio::pids_playing_audio() } else { HashSet::new() };
        let clip = if cfg.skip_if_clipboard_owner { win::clipboard_owner_pid() } else { None };

        let mut statuses = HashMap::new();
        for rule in &cfg.rules {
            let group = &groups[&rule.exe];
            let status = self.tick_rule(rule, group, &cfg, &windows, fg, clip, &audio, frozen.get(&rule.exe), now);
            statuses.insert(rule.exe.clone(), status);
        }
        self.rt.retain(|exe, _| rule_exes.contains(exe));

        if self.last_apps_scan.is_none_or(|t| now - t >= APPS_SCAN_EVERY) {
            self.last_apps_scan = Some(now);
            self.apps_cache = scan_apps(&tree, &windows, &excluded, &rule_exes);
        }

        let frozen_now = freezer::frozen_info();
        shared::shared().status = Status {
            apps: self.apps_cache.clone(),
            rules: statuses,
            total_saved_bytes: frozen_now.iter().map(|f| f.saved_bytes).sum(),
            frozen_count: frozen_now.len(),
        };
    }

    #[allow(clippy::too_many_arguments)]
    fn tick_rule(
        &mut self,
        rule: &Rule,
        group: &[u32],
        cfg: &Config,
        windows: &HashMap<u32, Vec<AppWindow>>,
        fg: Option<u32>,
        clip: Option<u32>,
        audio: &HashSet<u32>,
        frozen: Option<&freezer::FrozenInfo>,
        now: Instant,
    ) -> RuleStatus {
        let ncpu = self.ncpu;
        let rt = self.rt.entry(rule.exe.clone()).or_default();

        if let Some(f) = frozen {
            // Thaw path #2 (poll): it's in front even though the hook missed it.
            // Thaw path #3: a new process of the app appeared — the user (or the
            // system) is trying to use it, and it would hang talking to the frozen one.
            let in_front = fg.is_some_and(|p| f.pids.contains(&p));
            let new_proc = group.iter().any(|p| !f.pids.contains(p));
            if in_front || new_proc {
                freezer::thaw(&rule.exe, if in_front { "it's in front" } else { "it started a new process" });
                rt.cond_since = None;
                shared::repaint_ui();
            } else {
                return RuleStatus {
                    state: RuleState::Frozen { for_secs: f.since.elapsed().as_secs(), saved_bytes: f.saved_bytes },
                    procs: f.procs,
                    mem_bytes: measure(group).0,
                    cpu_percent: 0.0,
                };
            }
        }

        if group.is_empty() {
            *rt = RuleRuntime::default();
            return RuleStatus { state: RuleState::NotRunning, procs: 0, mem_bytes: 0, cpu_percent: 0.0 };
        }

        let (mem_bytes, cpu_total) = measure(group);
        match rt.cpu_sample {
            Some((t0, c0)) if now - t0 >= Duration::from_millis(900) => {
                let secs = (now - t0).as_secs_f64();
                let used = cpu_total.saturating_sub(c0) as f64 / 10_000_000.0; // 100ns -> s
                rt.cpu_percent = Some((used / secs / ncpu * 100.0) as f32);
                rt.cpu_sample = Some((now, cpu_total));
            }
            Some(_) => {}
            None => rt.cpu_sample = Some((now, cpu_total)),
        }
        if group.iter().any(|p| audio.contains(p)) {
            rt.last_audio = Some(now);
        }

        let procs = group.len();
        let cpu_percent = rt.cpu_percent.unwrap_or(0.0);
        let status = move |state| RuleStatus { state, procs, mem_bytes, cpu_percent };

        if !rule.enabled {
            rt.cond_since = None;
            return status(RuleState::Disabled);
        }
        if cfg.paused {
            rt.cond_since = None;
            return status(RuleState::Paused);
        }
        if let (Some(t), Some(err)) = (rt.retry_after, &rt.last_error)
            && now < t {
                return status(RuleState::Error(err.clone()));
            }

        if let Err(reason) = condition(rule, group, cfg, windows, fg, clip, rt, now) {
            rt.cond_since = None;
            return status(RuleState::Waiting(reason));
        }

        let since = *rt.cond_since.get_or_insert(now);
        let delay = if rule.minutes == 0 { GRACE } else { Duration::from_secs(rule.minutes as u64 * 60) };
        let elapsed = now - since;
        if elapsed < delay {
            return status(RuleState::Counting(delay - elapsed));
        }

        match freezer::freeze_group(&rule.exe, group, rule.trim) {
            Ok(saved) => {
                rt.last_error = None;
                rt.retry_after = None;
                // Race guard: if the user switched to it while we were freezing, undo.
                if win::foreground_pid().is_some_and(|p| group.contains(&p)) {
                    freezer::thaw(&rule.exe, "switched to it while dozing it");
                    rt.cond_since = None;
                    return status(RuleState::Waiting("In use".into()));
                }
                shared::repaint_ui();
                status(RuleState::Frozen { for_secs: 0, saved_bytes: saved })
            }
            Err(e) => {
                logln!("could not doze {}: {e}", rule.exe);
                rt.last_error = Some(e.clone());
                rt.retry_after = Some(now + RETRY_AFTER_ERROR);
                rt.cond_since = None;
                status(RuleState::Error(e))
            }
        }
    }
}

/// Ok(()) if the rule's freeze condition holds right now, else the reason it doesn't.
#[allow(clippy::too_many_arguments)]
fn condition(
    rule: &Rule,
    group: &[u32],
    cfg: &Config,
    windows: &HashMap<u32, Vec<AppWindow>>,
    fg: Option<u32>,
    clip: Option<u32>,
    rt: &RuleRuntime,
    now: Instant,
) -> Result<(), String> {
    let owns = |pid: Option<u32>| pid.is_some_and(|p| group.contains(&p));
    if owns(fg) {
        return Err("In use".into());
    }
    let wins: Vec<&AppWindow> = group.iter().filter_map(|p| windows.get(p)).flatten().collect();
    if wins.is_empty() {
        // Tray-only apps can't be woken by switching to them, so leave them alone.
        return Err("No open windows".into());
    }
    match rule.mode {
        Mode::Minimized => {
            if !wins.iter().all(|w| w.minimized) {
                return Err("Window not minimized".into());
            }
        }
        Mode::Unfocused => {}
        Mode::Idle => match rt.cpu_percent {
            None => return Err("Measuring CPU…".into()),
            Some(pct) if pct >= cfg.idle_cpu_percent => return Err(format!("Busy ({pct:.1}% CPU)")),
            Some(_) => {}
        },
    }
    if cfg.skip_if_playing_audio && rt.last_audio.is_some_and(|t| now - t < AUDIO_HOLD) {
        return Err("Playing audio".into());
    }
    if cfg.skip_if_clipboard_owner && owns(clip) {
        return Err("Owns the clipboard".into());
    }
    Ok(())
}

/// (working set bytes, total CPU time in 100ns) summed over the group.
fn measure(group: &[u32]) -> (u64, u64) {
    let mut mem = 0;
    let mut cpu = 0;
    for &pid in group {
        if let Ok(h) = win::open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
            mem += win::working_set_bytes(&h).unwrap_or(0);
            cpu += win::process_times(&h).map(|t| t.1).unwrap_or(0);
        }
    }
    (mem, cpu)
}

fn scan_apps(
    tree: &ProcTree,
    windows: &HashMap<u32, Vec<AppWindow>>,
    excluded: &HashSet<String>,
    rule_exes: &HashSet<String>,
) -> Vec<AppInfo> {
    let mut exes: Vec<String> = windows
        .keys()
        .filter_map(|pid| tree.by_pid.get(pid).map(|p| p.exe.clone()))
        .filter(|exe| !win::is_protected(exe))
        .collect();
    exes.sort();
    exes.dedup();
    let mut apps: Vec<AppInfo> = exes
        .into_iter()
        .map(|exe| {
            let group = tree.group(&exe, windows, excluded, rule_exes);
            let title = group
                .iter()
                .filter_map(|p| windows.get(p))
                .flatten()
                .map(|w| w.title.clone())
                .next()
                .unwrap_or_default();
            AppInfo { name: display_name(&exe), title, procs: group.len(), mem_bytes: measure(&group).0, exe }
        })
        .collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.mem_bytes));
    apps
}

pub fn display_name(exe: &str) -> String {
    let stem = exe.strip_suffix(".exe").unwrap_or(exe);
    let mut c = stem.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => stem.to_string(),
    }
}

// ---------------------------------------------------------------------------

struct ProcTree<'a> {
    by_pid: HashMap<u32, &'a ProcEntry>,
    children: HashMap<u32, Vec<u32>>,
    create_times: RefCell<HashMap<u32, Option<u64>>>,
}

impl<'a> ProcTree<'a> {
    fn new(procs: &'a [ProcEntry]) -> Self {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in procs {
            if p.ppid != p.pid {
                children.entry(p.ppid).or_default().push(p.pid);
            }
        }
        ProcTree { by_pid: procs.iter().map(|p| (p.pid, p)).collect(), children, create_times: RefCell::default() }
    }

    fn create_time(&self, pid: u32) -> Option<u64> {
        *self.create_times.borrow_mut().entry(pid).or_insert_with(|| {
            win::open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok().and_then(|h| win::process_times(&h)).map(|t| t.0)
        })
    }

    /// Every process of `exe`, plus its descendants — except descendants that are
    /// really something else: excluded children (terminals/shells and all under
    /// them), protected system processes, apps that have their own windows (e.g. a
    /// browser opened by clicking a link) and apps that have their own rule.
    fn group(
        &self,
        exe: &str,
        windows: &HashMap<u32, Vec<AppWindow>>,
        excluded: &HashSet<String>,
        rule_exes: &HashSet<String>,
    ) -> Vec<u32> {
        if win::is_protected(exe) {
            return Vec::new();
        }
        let mut stack: Vec<u32> = self.by_pid.values().filter(|p| p.exe == exe).map(|p| p.pid).collect();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            out.push(pid);
            for &child in self.children.get(&pid).map(Vec::as_slice).unwrap_or(&[]) {
                let Some(c) = self.by_pid.get(&child) else { continue };
                if c.exe != exe
                    && (excluded.contains(&c.exe)
                        || win::is_protected(&c.exe)
                        || rule_exes.contains(&c.exe)
                        || windows.contains_key(&child))
                {
                    continue;
                }
                // PID-reuse guard: a real child is never older than its parent.
                if let (Some(pt), Some(ct)) = (self.create_time(pid), self.create_time(child))
                    && ct < pt {
                        continue;
                    }
                stack.push(child);
            }
        }
        out.sort_unstable();
        out
    }
}
