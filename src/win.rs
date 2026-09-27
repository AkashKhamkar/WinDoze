//! Thin, safe-ish wrappers over the Win32 calls WinDoze needs.

use std::collections::HashMap;
use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::DataExchange::GetClipboardOwner;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::ProcessStatus::{EmptyWorkingSet, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOT, GW_OWNER, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindow,
    GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SwitchToThisWindow,
    WS_EX_TOOLWINDOW, WindowFromPoint,
};
use windows::core::{BOOL, PCSTR, PWSTR, w};

/// Processes that must never be frozen, even if the user asks.
/// Freezing any of these can hang the desktop, input, audio or security.
const PROTECTED: &[&str] = &[
    "system", "registry", "smss.exe", "csrss.exe", "wininit.exe", "winlogon.exe", "services.exe",
    "lsass.exe", "lsaiso.exe", "svchost.exe", "dwm.exe", "explorer.exe", "sihost.exe", "ctfmon.exe",
    "fontdrvhost.exe", "audiodg.exe", "conhost.exe", "openconsole.exe", "runtimebroker.exe",
    "searchhost.exe", "searchapp.exe", "searchui.exe", "startmenuexperiencehost.exe",
    "shellexperiencehost.exe", "shellhost.exe", "textinputhost.exe", "applicationframehost.exe",
    "lockapp.exe", "logonui.exe", "taskmgr.exe", "procexp.exe", "procexp64.exe", "msmpeng.exe",
    "mpdefendercoreservice.exe", "nissrv.exe", "securityhealthservice.exe", "securityhealthsystray.exe",
    "smartscreen.exe", "dllhost.exe", "wudfhost.exe", "vmmem", "vmmemwsl", "vmcompute.exe",
    "vmwp.exe", "wslservice.exe", "wsl.exe", "wslhost.exe", "widgets.exe", "phoneexperiencehost.exe",
    "systemsettings.exe", "userinit.exe", "spoolsv.exe", "taskhostw.exe", "consent.exe",
];

pub fn is_protected(exe: &str) -> bool {
    PROTECTED.contains(&exe) || exe == own_exe_name()
}

pub fn own_exe_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
            .unwrap_or_else(|| "windoze.exe".into())
    })
}

// ---------------------------------------------------------------------------
// Handles

/// A process handle that is closed on drop. Holding it also keeps the PID from
/// being reused, which is why frozen processes keep theirs open.
pub struct OwnedHandle(pub HANDLE);

// A HANDLE is just a kernel object reference; it's fine to use from any thread.
unsafe impl Send for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> windows::core::Result<OwnedHandle> {
    unsafe { OpenProcess(access, false, pid).map(OwnedHandle) }
}

fn filetime_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// (creation time, total CPU time) in 100ns units.
pub fn process_times(h: &OwnedHandle) -> Option<(u64, u64)> {
    let (mut c, mut e, mut k, mut u) = Default::default();
    unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u).ok()? };
    Some((filetime_u64(c), filetime_u64(k) + filetime_u64(u)))
}

pub fn working_set_bytes(h: &OwnedHandle) -> Option<u64> {
    let mut pmc = PROCESS_MEMORY_COUNTERS::default();
    let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe { GetProcessMemoryInfo(h.0, &mut pmc, cb).ok()? };
    Some(pmc.WorkingSetSize as u64)
}

pub fn is_running(h: &OwnedHandle) -> bool {
    const STILL_ACTIVE: u32 = 259;
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(h.0, &mut code).is_ok() && code == STILL_ACTIVE }
}

/// End a process immediately (like "End task"). Returns false if Windows refused.
pub fn terminate(pid: u32) -> bool {
    match open_process(pid, PROCESS_TERMINATE) {
        Ok(h) => unsafe { TerminateProcess(h.0, 1).is_ok() },
        Err(_) => false,
    }
}

pub fn empty_working_set(h: &OwnedHandle) -> bool {
    unsafe { EmptyWorkingSet(h.0).is_ok() }
}

// ---------------------------------------------------------------------------
// Undocumented-but-stable exports, resolved at runtime

type NtProcessFn = unsafe extern "system" fn(HANDLE) -> i32;
type HungFromGhostFn = unsafe extern "system" fn(HWND) -> HWND;

fn resolve(module: windows::core::PCWSTR, name: &'static [u8], load: bool) -> Option<usize> {
    unsafe {
        let m = if load { LoadLibraryW(module).ok()? } else { GetModuleHandleW(module).ok()? };
        GetProcAddress(m, PCSTR(name.as_ptr())).map(|f| f as usize)
    }
}

fn nt_suspend() -> Option<NtProcessFn> {
    static F: OnceLock<Option<usize>> = OnceLock::new();
    F.get_or_init(|| resolve(w!("ntdll.dll"), b"NtSuspendProcess\0", false))
        .map(|p| unsafe { std::mem::transmute::<usize, NtProcessFn>(p) })
}

fn nt_resume() -> Option<NtProcessFn> {
    static F: OnceLock<Option<usize>> = OnceLock::new();
    F.get_or_init(|| resolve(w!("ntdll.dll"), b"NtResumeProcess\0", false))
        .map(|p| unsafe { std::mem::transmute::<usize, NtProcessFn>(p) })
}

fn hung_from_ghost() -> Option<HungFromGhostFn> {
    static F: OnceLock<Option<usize>> = OnceLock::new();
    F.get_or_init(|| resolve(w!("user32.dll"), b"HungWindowFromGhostWindow\0", true))
        .map(|p| unsafe { std::mem::transmute::<usize, HungFromGhostFn>(p) })
}

/// Suspends every thread in the process. Returns the NTSTATUS on failure.
pub fn suspend(h: &OwnedHandle) -> Result<(), i32> {
    let f = nt_suspend().ok_or(-1)?;
    let status = unsafe { f(h.0) };
    if status >= 0 { Ok(()) } else { Err(status) }
}

pub fn resume(h: &OwnedHandle) -> Result<(), i32> {
    let f = nt_resume().ok_or(-1)?;
    let status = unsafe { f(h.0) };
    if status >= 0 { Ok(()) } else { Err(status) }
}

// ---------------------------------------------------------------------------
// Processes

#[derive(Clone, Debug)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    /// Lower-case exe file name, e.g. "figma.exe".
    pub exe: String,
}

/// All processes in the current user's session.
pub fn snapshot_processes() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let my_session = session_of(std::process::id());
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let snap = OwnedHandle(snap);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap.0, &mut entry).is_ok();
        while ok {
            let pid = entry.th32ProcessID;
            if pid != 0 && session_of(pid) == my_session {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                out.push(ProcEntry {
                    pid,
                    ppid: entry.th32ParentProcessID,
                    exe: String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase(),
                });
            }
            ok = Process32NextW(snap.0, &mut entry).is_ok();
        }
    }
    out
}

fn session_of(pid: u32) -> Option<u32> {
    let mut s = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut s).ok().map(|_| s) }
}

// ---------------------------------------------------------------------------
// Windows

#[derive(Clone, Debug)]
pub struct AppWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub minimized: bool,
    pub title: String,
}

/// Top-level windows a user would think of as "the app's windows":
/// visible, not cloaked (other virtual desktop / hidden UWP), not owned, not tool windows, titled.
pub fn app_windows() -> HashMap<u32, Vec<AppWindow>> {
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let out = &mut *(lparam.0 as *mut Vec<AppWindow>);
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            if GetWindow(hwnd, GW_OWNER).is_ok() {
                return BOOL(1);
            }
            if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL(1);
            }
            let mut cloaked: u32 = 0;
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut _,
                std::mem::size_of::<u32>() as u32,
            )
            .is_ok()
                && cloaked != 0
            {
                return BOOL(1);
            }
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(hwnd, &mut buf);
            if n <= 0 {
                return BOOL(1);
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            out.push(AppWindow {
                hwnd: hwnd.0 as isize,
                pid,
                minimized: IsIconic(hwnd).as_bool(),
                title: String::from_utf16_lossy(&buf[..n as usize]),
            });
            BOOL(1)
        }
    }
    let mut list: Vec<AppWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    let mut map: HashMap<u32, Vec<AppWindow>> = HashMap::new();
    for w in list {
        map.entry(w.pid).or_default().push(w);
    }
    map
}

fn window_pid(hwnd: HWND) -> Option<u32> {
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

/// PID owning `hwnd`. If Windows replaced a hung (frozen) window with a
/// "ghost" window, this resolves to the frozen app's PID instead of dwm's.
pub fn pid_for_window(hwnd: HWND) -> Option<u32> {
    if hwnd.is_invalid() {
        return None;
    }
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut class) };
    if n > 0 && String::from_utf16_lossy(&class[..n as usize]) == "Ghost"
        && let Some(f) = hung_from_ghost() {
            let hung = unsafe { f(hwnd) };
            if let Some(pid) = window_pid(hung) {
                return Some(pid);
            }
        }
    window_pid(hwnd)
}

pub fn foreground_pid() -> Option<u32> {
    pid_for_window(unsafe { GetForegroundWindow() })
}

pub fn foreground_window() -> HWND {
    unsafe { GetForegroundWindow() }
}

pub fn window_class(hwnd: HWND) -> String {
    let mut class = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut class) };
    if n > 0 { String::from_utf16_lossy(&class[..n as usize]) } else { String::new() }
}

/// Lower-case exe name of a process, e.g. "explorer.exe".
pub fn exe_of_pid(pid: u32) -> Option<String> {
    let h = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).ok()? };
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit('\\').next().map(|n| n.to_lowercase())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellUi {
    /// The taskbar itself (buttons, tray icons).
    Taskbar,
    /// Alt-Tab / Task View.
    Switcher,
    /// Other shell pop-ups: taskbar thumbnails, the tray overflow, jump lists.
    Other,
}

/// Is this window part of the Windows shell UI (as opposed to a File Explorer
/// window or the desktop, which also belong to explorer.exe)?
pub fn shell_ui(hwnd: HWND) -> Option<ShellUi> {
    if hwnd.is_invalid() {
        return None;
    }
    let pid = window_pid(hwnd)?;
    if exe_of_pid(pid).as_deref() != Some("explorer.exe") {
        return None;
    }
    match window_class(hwnd).as_str() {
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" => Some(ShellUi::Taskbar),
        "MultitaskingViewFrame" | "XamlExplorerHostIslandWindow" | "TaskSwitcherWnd" => Some(ShellUi::Switcher),
        "CabinetWClass" | "ExploreWClass" | "Progman" | "WorkerW" | "#32770" | "OperationStatusWindow" => None,
        _ => Some(ShellUi::Other),
    }
}

/// Top-level window under a screen point.
pub fn root_window_at(pt: POINT) -> HWND {
    unsafe { GetAncestor(WindowFromPoint(pt), GA_ROOT) }
}

/// Restore and bring a window to the front, the way Alt-Tab does.
pub fn restore_window(hwnd: isize) {
    unsafe { SwitchToThisWindow(HWND(hwnd as *mut _), true) };
}

pub fn clipboard_owner_pid() -> Option<u32> {
    unsafe { GetClipboardOwner().ok().and_then(window_pid) }
}

// ---------------------------------------------------------------------------
// Start with Windows (HKCU Run key)

pub fn set_start_with_windows(enable: bool) {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let name = w!("WinDoze");
    unsafe {
        if enable {
            let Ok(exe) = std::env::current_exe() else { return };
            let cmd = format!("\"{}\" --minimized", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let err = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key,
                name,
                REG_SZ.0,
                Some(wide.as_ptr() as *const _),
                (wide.len() * 2) as u32,
            );
            crate::logln!("start with Windows enabled (result {})", err.0);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, name);
            crate::logln!("start with Windows disabled");
        }
    }
}
