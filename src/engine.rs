//! The engine thread: watches focus/minimize/CPU/audio and decides when to
//! freeze and thaw. It owns a hidden top-level window so it gets a message
//! loop (needed for the foreground hook), a timer, the panic hotkey, and
//! WM_QUERYENDSESSION (so nothing stays frozen across logoff/shutdown).
//! None of this depends on the UI window being visible.
//!
//! Waking is the hard part: a dozing app can't respond when you click its
//! taskbar button or pick it in Alt-Tab, so Windows never brings it to the
//! front and there's no "it's in front now" event to react to. Instead we
//! watch for the *intent*: clicks (via a low-level mouse hook), what's under
//! the mouse on the taskbar and what's selected in Alt-Tab (via UI Automation).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_SWITCHEND, EVENT_SYSTEM_SWITCHSTART, GetMessageW, KillTimer, MSG,
    MSLLHOOKSTRUCT, PostMessageW, RegisterClassW, SetTimer, SetWindowsHookExW, TranslateMessage, WH_MOUSE_LL,
    WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_ENDSESSION, WM_HOTKEY,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_QUERYENDSESSION, WM_RBUTTONDOWN, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::w;

use crate::config::{Config, Mode, Rule};
use crate::freezer;
use crate::logln;
use crate::shared::{self, AppInfo, Command, RuleState, RuleStatus, Status};
use crate::uia::Uia;
use crate::win::{self, AppWindow, ProcEntry, ShellUi};

const WM_WAKE: u32 = WM_APP + 1;
/// Posted by the mouse hook: wparam = 1 for a left click, lparam = packed screen point.
const WM_CLICK: u32 = WM_APP + 2;
const TIMER_TICK: usize = 1;
const TIMER_SWITCHER: usize = 2;
const TIMER_FALLBACK: usize = 3;
const TIMER_RESTORE: usize = 4;
const TICK_MS: u32 = 1000;
/// If a taskbar click hasn't opened anything after this long, assume it was
/// meant for a dozing app we couldn't identify and wake them all.
const FALLBACK_MS: u32 = 700;
/// After waking an app because you clicked/picked it, make sure its window
/// actually came back after this long (the original request may have been lost).
const RESTORE_CHECK_MS: u32 = 400;
const HOTKEY_ID: i32 = 1;
/// "0 minutes" still waits this long, so a quick Alt-Tab never freezes anything.
const GRACE: Duration = Duration::from_secs(5);
/// Treat an app as "playing audio" for this long after its last sound.
const AUDIO_HOLD: Duration = Duration::from_secs(30);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(60);
const APPS_SCAN_EVERY: Duration = Duration::from_secs(3);
/// RAM counts as "low" when Windows has less than this share of it available.
/// Only then do we push dozing apps' memory out: with plenty free, trimming
/// gains nothing and just makes switching back slower (the app has to page
/// its memory back in).
const LOW_MEMORY_AVAILABLE: f64 = 0.30;
/// Keep the app you just copied from awake this long, so pasting from it works.
const CLIPBOARD_HOLD: Duration = Duration::from_secs(60);

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
    std::thread::Builder::new()
        .name("mouse-hook".into())
        .spawn(run_mouse_hook)
        .expect("failed to start mouse hook thread");
}

fn engine_hwnd() -> Option<HWND> {
    let h = ENGINE_HWND.load(Ordering::Acquire);
    (h != 0).then_some(HWND(h as *mut _))
}

/// A low-level mouse hook on its own thread. It must return fast (Windows
/// drops slow hooks, and a slow hook lags the whole mouse), so it only posts
/// the click position to the engine. It does nothing at all while no app is dozing.
fn run_mouse_hook() {
    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let down = matches!(wparam.0 as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN);
        if code >= 0 && down && freezer::any_frozen() {
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let packed = (info.pt.x as u32 as u64) | ((info.pt.y as u32 as u64) << 32);
            if let Some(engine) = engine_hwnd() {
                let left = (wparam.0 as u32 == WM_LBUTTONDOWN) as usize;
                let _ = unsafe { PostMessageW(Some(engine), WM_CLICK, WPARAM(left), LPARAM(packed as isize)) };
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Err(e) = SetWindowsHookExW(WH_MOUSE_LL, Some(proc), Some(hinstance.into()), 0) {
            logln!("WARNING: mouse hook failed ({e}); clicking a dozing app won't wake it");
            return;
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
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
        // Alt-Tab start/end and "window restored".
        let hook2 = SetWinEventHook(
            EVENT_SYSTEM_SWITCHSTART,
            EVENT_SYSTEM_MINIMIZEEND,
            None,
            Some(on_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if hook2.is_invalid() {
            logln!("WARNING: Alt-Tab hook failed");
        }
        if let Err(e) = RegisterHotKey(Some(hwnd), HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, 'T' as u32) {
            logln!("WARNING: could not register Ctrl+Alt+Shift+T ({e}); use the tray menu to wake apps");
        }
        SetTimer(Some(hwnd), TIMER_TICK, TICK_MS, None);

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

/// True if no new app window has come to the front since `fg_then`: it's
/// the same window, nothing, or still the taskbar / Alt-Tab.
fn nothing_new_in_front(fg_then: isize) -> bool {
    let fg_now = win::foreground_window();
    fg_now.is_invalid() || fg_now.0 as isize == fg_then || win::shell_ui(fg_now).is_some()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            match wparam.0 {
                TIMER_SWITCHER => with_engine(|e| e.poll_switcher()),
                TIMER_FALLBACK => with_engine(|e| e.fallback_fire()),
                TIMER_RESTORE => with_engine(|e| e.restore_fire()),
                _ => with_engine(|e| e.tick()),
            }
            LRESULT(0)
        }
        WM_WAKE => {
            with_engine(|e| e.tick());
            LRESULT(0)
        }
        WM_CLICK => {
            let packed = lparam.0 as u64;
            let pt = POINT { x: packed as u32 as i32, y: (packed >> 32) as u32 as i32 };
            with_engine(|e| e.on_click(pt, wparam.0 == 1));
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
    match event {
        EVENT_SYSTEM_FOREGROUND => {
            let mut handled = false;
            with_engine(|e| {
                e.on_foreground(hwnd);
                handled = true;
            });
            // Engine busy (shouldn't happen): still wake the app if it's the one in front.
            if !handled
                && let Some(pid) = win::pid_for_window(hwnd)
                && let Some(exe) = freezer::frozen_exe_for_pid(pid)
            {
                freezer::thaw(&exe, "switched to it");
                wake();
            }
        }
        EVENT_SYSTEM_SWITCHSTART => with_engine(|e| e.switcher_begin()),
        EVENT_SYSTEM_SWITCHEND => with_engine(|e| e.switcher_end()),
        EVENT_SYSTEM_MINIMIZEEND => {
            if let Some(pid) = win::pid_for_window(hwnd)
                && let Some(exe) = freezer::frozen_exe_for_pid(pid)
            {
                with_engine(|e| e.wake_app(&exe, "its window was restored", false));
            }
        }
        _ => {}
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
    uia: Option<Uia>,
    uia_tried: bool,
    /// Set while Alt-Tab / Task View is open.
    switcher: Option<SwitcherState>,
    /// Foreground window at the time of an unidentified taskbar click.
    fallback_fg: Option<isize>,
    /// Last foreground window that was an app (not the taskbar / Alt-Tab).
    last_app_fg: isize,
    /// Last clipboard sequence number seen, and when it last changed (= a copy).
    clip_seq: u32,
    clip_copied_at: Option<Instant>,
    /// Apps just woken by a click/pick whose windows we should make sure come back.
    restore_pending: Vec<(String, HashSet<u32>, isize)>,
}

struct SwitcherState {
    started: Instant,
    /// Dozing app currently highlighted in the switcher, if any.
    candidate: Option<String>,
    /// Whether UI Automation told us anything at all (if not, it's broken here).
    saw_names: bool,
    last_names: Vec<String>,
}

impl Engine {
    fn new() -> Self {
        Engine {
            rt: HashMap::new(),
            ncpu: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64,
            last_apps_scan: None,
            apps_cache: Vec::new(),
            uia: None,
            uia_tried: false,
            switcher: None,
            fallback_fg: None,
            last_app_fg: 0,
            clip_seq: win::clipboard_sequence(),
            clip_copied_at: None,
            restore_pending: Vec::new(),
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

    // -----------------------------------------------------------------------
    // Waking: working out that the user wants a dozing app back.

    fn uia(&mut self) -> Option<&Uia> {
        if !self.uia_tried {
            self.uia_tried = true;
            self.uia = Uia::new();
            if self.uia.is_none() {
                logln!("WARNING: UI Automation unavailable; taskbar/Alt-Tab waking falls back to waking all apps");
            }
        }
        self.uia.as_ref()
    }

    /// Wake one dozing app. With `restore`, also make sure its window comes
    /// back to the front, since the click/pick that was meant to do that went
    /// to an app that couldn't respond.
    fn wake_app(&mut self, exe: &str, reason: &str, restore: bool) {
        let pids = freezer::frozen_info().into_iter().find(|f| f.exe == exe).map(|f| f.pids).unwrap_or_default();
        if !freezer::thaw(exe, reason) {
            return;
        }
        self.reset_rule(exe);
        if restore && !pids.is_empty() {
            let fg = win::foreground_window().0 as isize;
            self.restore_pending.push((exe.to_string(), pids, fg));
            if let Some(h) = engine_hwnd() {
                unsafe { SetTimer(Some(h), TIMER_RESTORE, RESTORE_CHECK_MS, None) };
            }
        }
        wake();
        shared::repaint_ui();
    }

    fn on_foreground(&mut self, hwnd: HWND) {
        if let Some(pid) = win::pid_for_window(hwnd)
            && let Some(exe) = freezer::frozen_exe_for_pid(pid)
        {
            self.wake_app(&exe, "switched to it", false);
        }
        match win::shell_ui(hwnd) {
            Some(ShellUi::Switcher) => self.switcher_begin(),
            Some(_) => {}
            None => {
                if self.switcher.is_some() {
                    self.switcher_end();
                }
                self.last_app_fg = hwnd.0 as isize;
            }
        }
    }

    /// A mouse button went down somewhere while at least one app is dozing.
    fn on_click(&mut self, pt: POINT, left: bool) {
        let root = win::root_window_at(pt);
        // Clicked straight on a dozing app's window.
        if let Some(pid) = win::pid_for_window(root)
            && let Some(exe) = freezer::frozen_exe_for_pid(pid)
        {
            self.wake_app(&exe, "clicked its window", false);
            return;
        }
        // Clicked the taskbar, a thumbnail, the tray, or an item in Alt-Tab / Task View:
        // which app was it for?
        if win::shell_ui(root).is_none() {
            return;
        }
        let names = self.uia().map(|u| u.names_at(pt)).unwrap_or_default();
        match self.match_app(&names) {
            Some((exe, true)) => {
                logln!("taskbar click on {names:?} -> {exe}");
                // A right-click opens the jump list; don't pop the window up for that.
                self.wake_app(&exe, "clicked it on the taskbar", left);
            }
            Some((_, false)) => {} // an app that isn't dozing; Windows handles it
            None => {
                // Only an app's taskbar button ("Figma - 1 running window"), or a
                // click we couldn't read at all, gets the safety net; not Start,
                // the clock, empty taskbar space, etc.
                let app_button =
                    names.is_empty() || names.iter().any(|n| n.to_lowercase().contains("running window"));
                if app_button {
                    logln!("taskbar click on {names:?}: no app matched, waiting to see if anything opens");
                    self.fallback_fg = Some(win::foreground_window().0 as isize);
                    if let Some(h) = engine_hwnd() {
                        unsafe { SetTimer(Some(h), TIMER_FALLBACK, FALLBACK_MS, None) };
                    }
                }
            }
        }
    }

    fn fallback_fire(&mut self) {
        if let Some(h) = engine_hwnd() {
            let _ = unsafe { KillTimer(Some(h), TIMER_FALLBACK) };
        }
        let Some(fg_then) = self.fallback_fg.take() else { return };
        // Nothing new came to the front (still the same window, or still the
        // taskbar / switcher): the click was most likely for a dozing app we
        // couldn't identify. Waking everything beats leaving you stuck.
        if nothing_new_in_front(fg_then) && freezer::any_frozen() {
            freezer::thaw_all("a click didn't open anything, so it was probably meant for a dozing app");
            self.reset_all();
            wake();
            shared::repaint_ui();
        }
    }

    fn switcher_begin(&mut self) {
        if self.switcher.is_some() || !freezer::any_frozen() {
            return;
        }
        logln!("switcher opened (foreground: {})", win::window_class(win::foreground_window()));
        self.switcher =
            Some(SwitcherState { started: Instant::now(), candidate: None, saw_names: false, last_names: Vec::new() });
        if let Some(h) = engine_hwnd() {
            unsafe { SetTimer(Some(h), TIMER_SWITCHER, 100, None) };
        }
        self.poll_switcher();
    }

    /// While Alt-Tab / Task View is open, track which window is highlighted.
    fn poll_switcher(&mut self) {
        let Some(started) = self.switcher.as_ref().map(|s| s.started) else {
            self.stop_switcher_timer();
            return;
        };
        if started.elapsed() > Duration::from_secs(60) {
            self.switcher = None;
            self.stop_switcher_timer();
            return;
        }
        // Only ask for focus while the switcher itself is in front, never a (possibly dozing) app.
        if win::shell_ui(win::foreground_window()) != Some(ShellUi::Switcher) {
            // The switcher has closed without handing focus to an app (e.g. you
            // picked a dozing one): act on what was highlighted.
            if self.switcher.as_ref().is_some_and(|s| s.saw_names) {
                self.switcher_end();
            }
            return;
        }
        let names = self.uia().map(|u| u.focused_names()).unwrap_or_default();
        if names.is_empty() {
            return;
        }
        let matched = self.match_app(&names);
        if let Some(state) = self.switcher.as_mut() {
            state.saw_names = true;
            state.last_names = names;
            state.candidate = match matched {
                Some((exe, true)) => Some(exe),
                _ => None,
            };
        }
    }

    fn switcher_end(&mut self) {
        self.stop_switcher_timer();
        let Some(state) = self.switcher.take() else { return };
        logln!(
            "switcher closed: highlighted {:?} -> {:?} (read names: {})",
            state.last_names,
            state.candidate,
            state.saw_names
        );
        // If a different, awake app came to the front, that's what you picked
        // (the highlight just hadn't caught up); leave the dozing one alone.
        let fg = win::foreground_window();
        let picked_other = !fg.is_invalid()
            && win::shell_ui(fg).is_none()
            && fg.0 as isize != self.last_app_fg
            && win::pid_for_window(fg).is_some_and(|p| freezer::frozen_exe_for_pid(p).is_none());
        if picked_other {
            return;
        }
        if let Some(exe) = state.candidate {
            self.wake_app(&exe, "picked it in Alt-Tab", true);
        } else if !state.saw_names && freezer::any_frozen() {
            // UI Automation told us nothing, so we can't see what was picked. If we
            // end up back where we started, assume it was a dozing app.
            self.fallback_fg = Some(self.last_app_fg);
            if let Some(h) = engine_hwnd() {
                unsafe { SetTimer(Some(h), TIMER_FALLBACK, FALLBACK_MS, None) };
            }
        }
    }

    fn stop_switcher_timer(&self) {
        if let Some(h) = engine_hwnd() {
            let _ = unsafe { KillTimer(Some(h), TIMER_SWITCHER) };
        }
    }

    /// After a click/pick woke an app, bring its window back if Windows didn't.
    fn restore_fire(&mut self) {
        if let Some(h) = engine_hwnd() {
            let _ = unsafe { KillTimer(Some(h), TIMER_RESTORE) };
        }
        let pending = std::mem::take(&mut self.restore_pending);
        if pending.is_empty() {
            return;
        }
        let windows = win::app_windows();
        let fg = win::foreground_pid();
        for (exe, pids, fg_at_wake) in pending {
            if fg.is_some_and(|p| pids.contains(&p)) {
                continue; // it came back by itself
            }
            if !nothing_new_in_front(fg_at_wake) {
                continue; // you've moved on to something else; don't steal focus
            }
            let wins: Vec<&AppWindow> = pids.iter().filter_map(|p| windows.get(p)).flatten().collect();
            let target = wins.iter().find(|w| w.minimized).or(wins.first());
            if let Some(w) = target {
                logln!("bringing {exe} to the front");
                win::restore_window(w.hwnd);
            }
        }
    }

    /// Which app do these taskbar / Alt-Tab names refer to? Matches app names
    /// ("figma") and window titles; the longest match wins. Returns (exe, is_dozing).
    fn match_app(&self, names: &[String]) -> Option<(String, bool)> {
        if names.is_empty() {
            return None;
        }
        let frozen = freezer::frozen_info();
        let windows = win::app_windows();
        let stem = |exe: &str| exe.strip_suffix(".exe").unwrap_or(exe).to_lowercase();
        let mut apps: Vec<(String, bool, Vec<String>)> = frozen
            .iter()
            .map(|f| {
                let mut keys = vec![stem(&f.exe)];
                keys.extend(f.pids.iter().filter_map(|p| windows.get(p)).flatten().map(|w| w.title.to_lowercase()));
                (f.exe.clone(), true, keys)
            })
            .collect();
        for app in &self.apps_cache {
            if !frozen.iter().any(|f| f.exe == app.exe) {
                apps.push((app.exe.clone(), false, vec![stem(&app.exe), app.title.to_lowercase()]));
            }
        }
        let mut best: Option<(usize, &str, bool)> = None;
        for name in names.iter().map(|n| n.to_lowercase()) {
            for (exe, dozing, keys) in &apps {
                for key in keys.iter().map(|k| k.trim()) {
                    if key.len() >= 3 && name.contains(key) && best.is_none_or(|b| key.len() > b.0) {
                        best = Some((key.len(), exe, *dozing));
                    }
                }
            }
        }
        best.map(|(_, exe, dozing)| (exe.to_string(), dozing))
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

        freezer::prune_exited();
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
                Command::Kill(exe) => {
                    let mut pids: HashSet<u32> = group_of(&exe).into_iter().collect();
                    if let Some(f) = freezer::frozen_info().into_iter().find(|f| f.exe == exe) {
                        pids.extend(f.pids);
                    }
                    let failed = pids.iter().filter(|&&p| !win::terminate(p)).count();
                    // Resume anything that survived, so nothing is left stuck asleep.
                    freezer::thaw(&exe, "force quit");
                    self.reset_rule(&exe);
                    logln!("force quit {exe}: {} of {} processes ended", pids.len() - failed, pids.len());
                    if failed > 0 {
                        let rt = self.rt.entry(exe.clone()).or_default();
                        rt.last_error = Some(format!(
                            "Couldn't force quit {failed} process(es). Is it running as administrator?"
                        ));
                        rt.retry_after = Some(now + Duration::from_secs(10));
                    }
                    shared::repaint_ui();
                }
                Command::FreezeNow(exe) => {
                    let trim = cfg.rule(&exe).map(|r| r.trim).unwrap_or(true) && memory_low();
                    let rt = self.rt.entry(exe.clone()).or_default();
                    match freezer::freeze_group(&exe, &group_of(&exe), trim) {
                        Ok(_) => rt.last_error = None,
                        Err(e) => {
                            logln!("manual doze of {exe} failed: {e}");
                            rt.last_error = Some(format!("Can't doze: {e}"));
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
        // Only the app you copied from *recently* is kept awake: pastes almost
        // always happen within a minute, and after that it may doze like anything else.
        let seq = win::clipboard_sequence();
        if seq != self.clip_seq {
            self.clip_seq = seq;
            self.clip_copied_at = Some(now);
        }
        let recent_copy = self.clip_copied_at.is_some_and(|t| now - t < CLIPBOARD_HOLD);
        let clip = if cfg.skip_if_clipboard_owner && recent_copy { win::clipboard_owner_pid() } else { None };

        let mut statuses = HashMap::new();
        for rule in &cfg.rules {
            let group = &groups[&rule.exe];
            let status = self.tick_rule(rule, group, &cfg, &windows, fg, clip, &audio, frozen.get(&rule.exe), now);
            statuses.insert(rule.exe.clone(), status);
        }
        self.rt.retain(|exe, _| rule_exes.contains(exe));

        // RAM got tight after some apps dozed: push out the memory of the one
        // that's been asleep longest (one per tick to spread the disk work).
        if memory_low() {
            let oldest_untrimmed = freezer::frozen_info()
                .into_iter()
                .filter(|f| !f.trimmed && cfg.rule(&f.exe).is_some_and(|r| r.trim))
                .min_by_key(|f| f.since);
            if let Some(f) = oldest_untrimmed {
                logln!("RAM is low; freeing memory of {}, which is already dozing", f.exe);
                freezer::trim_dozing(&f.exe);
            }
        }

        if self.last_apps_scan.is_none_or(|t| now - t >= APPS_SCAN_EVERY) {
            self.last_apps_scan = Some(now);
            self.apps_cache = scan_apps(&tree, &windows, &excluded, &rule_exes);
        }

        let frozen_now = freezer::frozen_info();
        let (system_total, system_available) = win::system_memory().unwrap_or((0, 0));
        shared::shared().status = Status {
            apps: self.apps_cache.clone(),
            rules: statuses,
            total_saved_bytes: frozen_now.iter().map(|f| f.saved_bytes).sum(),
            frozen_count: frozen_now.len(),
            system_total,
            system_available,
            memory_low: memory_low(),
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
                    state: RuleState::Frozen {
                        for_secs: f.since.elapsed().as_secs(),
                        saved_bytes: f.saved_bytes,
                        trimmed: f.trimmed,
                    },
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

        let trim = rule.trim && memory_low();
        match freezer::freeze_group(&rule.exe, group, trim) {
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
                status(RuleState::Frozen { for_secs: 0, saved_bytes: saved, trimmed: trim })
            }
            Err(e) => {
                logln!("could not doze {}: {e}", rule.exe);
                let e = format!("Can't doze: {e}");
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
        return Err("You just copied from it".into());
    }
    Ok(())
}

/// (private memory bytes, total CPU time in 100ns) summed over the group.
fn measure(group: &[u32]) -> (u64, u64) {
    let mut mem = 0;
    let mut cpu = 0;
    for &pid in group {
        if let Ok(h) = win::open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
            mem += win::private_memory_bytes(&h).unwrap_or(0);
            cpu += win::process_times(&h).map(|t| t.1).unwrap_or(0);
        }
    }
    (mem, cpu)
}

/// Is Windows short on RAM right now?
fn memory_low() -> bool {
    match win::system_memory() {
        Some((total, avail)) if total > 0 => (avail as f64) < total as f64 * LOW_MEMORY_AVAILABLE,
        _ => true, // can't tell: behave as before and free memory
    }
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
