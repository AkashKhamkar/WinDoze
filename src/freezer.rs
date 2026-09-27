//! Freezing and thawing process groups, plus the crash-safety journal.
//!
//! Invariants:
//! * A group is frozen all-or-nothing: if any member can't be suspended, the
//!   ones already suspended are resumed and the freeze is reported as failed.
//! * Every frozen PID (+ its creation time) is written to `frozen.json` before
//!   we return, so a crash, a kill from Task Manager, or a reboot-less restart
//!   can always resume it (see `recover_from_journal` and the watchdog).
//! * We keep each frozen process's handle open, so its PID can't be reused
//!   while it's frozen.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use windows::Win32::System::Threading::{
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_SUSPEND_RESUME,
};

use crate::logln;
use crate::win::{self, OwnedHandle};

struct FrozenProc {
    pid: u32,
    create_time: u64,
    handle: OwnedHandle,
}

pub struct FrozenGroup {
    pub exe: String,
    procs: Vec<FrozenProc>,
    pub since: Instant,
    /// Private memory released by trimming (0 if not trimmed yet).
    pub saved_bytes: u64,
    /// Whether its memory has been pushed out of RAM yet. We only do that when
    /// RAM is actually tight; otherwise it stays put so waking is instant.
    pub trimmed: bool,
}

impl FrozenGroup {
    pub fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.procs.iter().map(|p| p.pid)
    }
    pub fn proc_count(&self) -> usize {
        self.procs.len()
    }
}

static FROZEN: Mutex<Vec<FrozenGroup>> = Mutex::new(Vec::new());
/// Lock-free "is anything dozing?" for the mouse hook, which must never block.
static ANY_FROZEN: AtomicBool = AtomicBool::new(false);

pub fn any_frozen() -> bool {
    ANY_FROZEN.load(Ordering::Relaxed)
}

fn update_flag(groups: &[FrozenGroup]) {
    ANY_FROZEN.store(!groups.is_empty(), Ordering::Relaxed);
}

fn lock() -> std::sync::MutexGuard<'static, Vec<FrozenGroup>> {
    FROZEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Summary for the UI / engine, without exposing handles.
pub struct FrozenInfo {
    pub exe: String,
    pub since: Instant,
    pub saved_bytes: u64,
    pub trimmed: bool,
    pub procs: usize,
    pub pids: HashSet<u32>,
}

pub fn frozen_info() -> Vec<FrozenInfo> {
    lock()
        .iter()
        .map(|g| FrozenInfo {
            exe: g.exe.clone(),
            since: g.since,
            saved_bytes: g.saved_bytes,
            trimmed: g.trimmed,
            procs: g.proc_count(),
            pids: g.pids().collect(),
        })
        .collect()
}

/// Which frozen app (if any) owns this PID.
pub fn frozen_exe_for_pid(pid: u32) -> Option<String> {
    lock().iter().find(|g| g.pids().any(|p| p == pid)).map(|g| g.exe.clone())
}

/// Freeze every PID in `pids` as one unit. Returns bytes trimmed.
pub fn freeze_group(exe: &str, pids: &[u32], trim: bool) -> Result<u64, String> {
    if pids.is_empty() {
        return Err("no processes".into());
    }
    if win::is_protected(exe) {
        return Err("protected system process".into());
    }
    let mut frozen = lock();
    if frozen.iter().any(|g| g.exe == exe) {
        return Ok(0);
    }

    // 1. Open everything first; bail before touching anything if we can't.
    // Trimming needs extra rights; if a process won't grant them we still freeze it, just untrimmed.
    let required = PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION;
    let with_trim = required | PROCESS_QUERY_INFORMATION | PROCESS_SET_QUOTA;
    let mut procs = Vec::with_capacity(pids.len());
    for &pid in pids {
        let handle = win::open_process(pid, with_trim)
            .or_else(|_| win::open_process(pid, required))
            .map_err(|e| {
                format!("can't open process {pid} ({}). Is the app running as administrator?", e.message())
            })?;
        let create_time = win::process_times(&handle).map(|t| t.0).unwrap_or(0);
        procs.push(FrozenProc { pid, create_time, handle });
    }

    // 2. Journal first, so even a crash mid-suspend is recoverable.
    let mut journal: Vec<JournalEntry> = journal_entries_of(&frozen);
    journal.extend(procs.iter().map(|p| JournalEntry { pid: p.pid, create_time: p.create_time }));
    write_journal(&journal);

    // 3. Suspend all; roll back on the first failure.
    for i in 0..procs.len() {
        if let Err(status) = win::suspend(&procs[i].handle) {
            for done in &procs[..i] {
                let _ = win::resume(&done.handle);
            }
            write_journal(&journal_entries_of(&frozen));
            return Err(format!("suspend failed for process {} (NTSTATUS {status:#x})", procs[i].pid));
        }
    }

    let mut group = FrozenGroup { exe: exe.to_string(), procs, since: Instant::now(), saved_bytes: 0, trimmed: false };
    logln!("dozed {exe}: {} processes", group.procs.len());
    if trim {
        trim_group(&mut group);
    }
    let saved = group.saved_bytes;
    frozen.push(group);
    update_flag(&frozen);
    Ok(saved)
}

/// Push a dozing app's private memory out of RAM (to compressed memory or
/// the pagefile). Its threads are suspended, so nothing can pull it back in
/// until it wakes: this is what makes the RAM genuinely available to other apps.
fn trim_group(g: &mut FrozenGroup) {
    let avail_before = win::system_memory().map(|m| m.1);
    let mut saved = 0u64;
    let mut failed = 0;
    for p in &g.procs {
        let before = win::private_memory_bytes(&p.handle).unwrap_or(0);
        if !win::empty_working_set(&p.handle) {
            failed += 1;
            continue;
        }
        let after = win::private_memory_bytes(&p.handle).unwrap_or(before);
        saved += before.saturating_sub(after);
    }
    g.saved_bytes += saved;
    g.trimmed = true;
    let avail_after = win::system_memory().map(|m| m.1);
    let mb = |b: u64| b as f64 / 1_048_576.0;
    logln!(
        "trimmed {}: released {:.0} MB of private memory{}; Windows available RAM {} -> {} MB (keeps rising for a few seconds as Windows compresses/writes the pages)",
        g.exe,
        mb(saved),
        if failed > 0 { format!(" ({failed} process(es) refused)") } else { String::new() },
        avail_before.map(|b| format!("{:.0}", mb(b))).unwrap_or("?".into()),
        avail_after.map(|b| format!("{:.0}", mb(b))).unwrap_or("?".into()),
    );
}

/// Trim an app that's already dozing (used when RAM gets tight after it dozed).
/// Returns bytes released, or None if it isn't dozing or was already trimmed.
pub fn trim_dozing(exe: &str) -> Option<u64> {
    let mut frozen = lock();
    let g = frozen.iter_mut().find(|g| g.exe == exe && !g.trimmed)?;
    let before = g.saved_bytes;
    trim_group(g);
    Some(g.saved_bytes - before)
}

fn resume_group(g: &FrozenGroup) {
    for p in &g.procs {
        if let Err(status) = win::resume(&p.handle) {
            logln!("resume failed for {} pid {} (NTSTATUS {status:#x})", g.exe, p.pid);
        }
    }
}

pub fn thaw(exe: &str, reason: &str) -> bool {
    let mut frozen = lock();
    let Some(idx) = frozen.iter().position(|g| g.exe == exe) else {
        return false;
    };
    let g = frozen.remove(idx);
    resume_group(&g);
    update_flag(&frozen);
    write_journal(&journal_entries_of(&frozen));
    logln!("woke {exe} ({reason}) after {}s", g.since.elapsed().as_secs());
    true
}

/// Forget dozing processes that no longer exist (the app was closed or force
/// quit). Without this a closed app would show as "Dozing" forever.
pub fn prune_exited() {
    let mut frozen = lock();
    let before = frozen.len();
    let mut changed = false;
    frozen.retain_mut(|g| {
        let n = g.procs.len();
        g.procs.retain(|p| win::is_running(&p.handle));
        changed |= g.procs.len() != n;
        if g.procs.is_empty() {
            logln!("{} exited while dozing", g.exe);
        }
        !g.procs.is_empty()
    });
    if changed || frozen.len() != before {
        update_flag(&frozen);
        write_journal(&journal_entries_of(&frozen));
    }
}

pub fn thaw_all(reason: &str) {
    let mut frozen = lock();
    for g in frozen.drain(..) {
        resume_group(&g);
        logln!("woke {} ({reason})", g.exe);
    }
    update_flag(&frozen);
    write_journal(&[]);
}

/// Used by the panic hook: must never deadlock.
pub fn emergency_thaw() {
    match FROZEN.try_lock() {
        Ok(mut g) => {
            for grp in g.drain(..) {
                resume_group(&grp);
            }
            write_journal(&[]);
        }
        Err(std::sync::TryLockError::Poisoned(p)) => {
            let mut g = p.into_inner();
            for grp in g.drain(..) {
                resume_group(&grp);
            }
            write_journal(&[]);
        }
        // Another thread holds the lock (and may be the one panicking).
        // Fall back to the journal, which opens fresh handles.
        Err(std::sync::TryLockError::WouldBlock) => recover_from_journal(),
    }
}

// ---------------------------------------------------------------------------
// Journal

#[derive(Serialize, Deserialize, Clone, Copy)]
struct JournalEntry {
    pid: u32,
    create_time: u64,
}

fn journal_path() -> PathBuf {
    crate::config::data_dir().join("dozing.json")
}

fn journal_entries_of(groups: &[FrozenGroup]) -> Vec<JournalEntry> {
    groups
        .iter()
        .flat_map(|g| g.procs.iter().map(|p| JournalEntry { pid: p.pid, create_time: p.create_time }))
        .collect()
}

fn write_journal(entries: &[JournalEntry]) {
    let path = journal_path();
    if entries.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string(entries)
        && std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
}

/// Resume anything a previous WinDoze instance left frozen.
/// Checks creation time so a reused PID is never touched.
pub fn recover_from_journal() {
    let path = journal_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let entries: Vec<JournalEntry> = serde_json::from_str(&text).unwrap_or_default();
    let mut resumed = 0;
    for e in &entries {
        let Ok(h) = win::open_process(e.pid, PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION) else {
            continue; // already exited
        };
        let same_process = win::process_times(&h).map(|t| t.0) == Some(e.create_time) || e.create_time == 0;
        if same_process && win::resume(&h).is_ok() {
            resumed += 1;
        }
    }
    let _ = std::fs::remove_file(&path);
    if !entries.is_empty() {
        logln!("recovery: resumed {resumed} of {} processes left dozing by a previous run", entries.len());
    }
}
