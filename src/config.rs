//! User settings, persisted as JSON in %APPDATA%\WinDoze\config.json.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// When an app should be frozen.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Mode {
    /// All of the app's windows are minimized.
    Minimized,
    /// The app is not the foreground app.
    Unfocused,
    /// Not in the foreground AND using almost no CPU AND not playing audio.
    Idle,
}

impl Mode {
    /// In the order shown in the dropdown; the first is the default.
    pub const ALL: [Mode; 3] = [Mode::Unfocused, Mode::Minimized, Mode::Idle];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Unfocused => "When I switch away",
            Mode::Minimized => "When minimized",
            Mode::Idle => "When idle in the background",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Mode::Unfocused => {
                "Dozes once you switch to another app (Alt-Tab, taskbar, clicking another window), even if its window is still visible."
            }
            Mode::Minimized => "Dozes only once every window of the app is minimized.",
            Mode::Idle => {
                "Dozes once you've switched away AND it's doing nothing in the background (CPU below the idle threshold, no audio)."
            }
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
pub struct Rule {
    /// Lower-case executable name, e.g. "figma.exe". This is the rule's identity.
    pub exe: String,
    pub display_name: String,
    pub enabled: bool,
    pub mode: Mode,
    /// Minutes the condition must hold before freezing. 0 = after a short grace period.
    pub minutes: u32,
    /// Also push the frozen app's memory out of RAM (into compressed memory / pagefile).
    pub trim: bool,
}

impl Rule {
    pub fn new(exe: &str, display_name: &str) -> Self {
        Rule {
            exe: exe.to_lowercase(),
            display_name: display_name.to_string(),
            enabled: true,
            mode: Mode::ALL[0],
            minutes: 0,
            trim: true,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    pub rules: Vec<Rule>,
    /// "Idle" means the app's processes together use less than this % of total CPU.
    pub idle_cpu_percent: f32,
    pub skip_if_playing_audio: bool,
    pub skip_if_clipboard_owner: bool,
    /// Child processes with these exe names (and everything under them) are never frozen.
    /// Keeps builds/dev servers in an editor's terminal running.
    pub excluded_children: Vec<String>,
    pub paused: bool,
    pub start_with_windows: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rules: Vec::new(),
            idle_cpu_percent: 1.0,
            skip_if_playing_audio: true,
            skip_if_clipboard_owner: true,
            excluded_children: [
                "cmd.exe", "powershell.exe", "pwsh.exe", "bash.exe", "sh.exe", "zsh.exe", "wsl.exe",
                "wslhost.exe", "conhost.exe", "openconsole.exe", "windowsterminal.exe", "mintty.exe",
                "ssh.exe", "docker.exe",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            paused: false,
            start_with_windows: false,
        }
    }
}

impl Config {
    pub fn rule(&self, exe: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.exe == exe)
    }
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("WinDoze");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn load() -> Config {
    match std::fs::read_to_string(config_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            crate::logln!("config.json is invalid ({e}); using defaults");
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

pub fn save(cfg: &Config) {
    let path = config_path();
    let tmp = path.with_extension("json.tmp");
    match serde_json::to_string_pretty(cfg) {
        Ok(text) => {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        Err(e) => crate::logln!("failed to serialize config: {e}"),
    }
}
