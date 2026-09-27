//! The settings window (egui) and the tray icon.
//!
//! Closing the window only hides it; WinDoze keeps running in the tray.
//! Everything that thaws apps (tray menu, hotkey, focus hook) works while the
//! window is hidden, because none of it goes through `ui()`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::{self, Config, Mode, Rule};
use crate::logln;
use crate::shared::{self, AppInfo, Command, RuleState, RuleStatus};

static SHOW_REQUESTED: AtomicBool = AtomicBool::new(false);

const FROZEN_BLUE: Color32 = Color32::from_rgb(96, 165, 250);
const AMBER: Color32 = Color32::from_rgb(245, 158, 11);
const RED: Color32 = Color32::from_rgb(239, 68, 68);

/// Thaw everything, save, and exit. Safe to call from any thread.
pub fn quit() -> ! {
    crate::freezer::thaw_all("WinDoze is quitting");
    let cfg = shared::shared().config.clone();
    config::save(&cfg);
    logln!("WinDoze exited");
    std::process::exit(0);
}

fn request_show() {
    SHOW_REQUESTED.store(true, Ordering::SeqCst);
    shared::repaint_ui();
}

pub fn run(start_hidden: bool) -> eframe::Result {
    let (rgba, size) = icon_rgba(64);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WinDoze")
            .with_inner_size([780.0, 680.0])
            .with_min_inner_size([600.0, 420.0])
            .with_icon(Arc::new(egui::IconData { rgba, width: size, height: size }))
            .with_visible(!start_hidden),
        ..Default::default()
    };
    eframe::run_native(
        "WinDoze",
        options,
        Box::new(|cc| {
            let _ = shared::UI_CTX.set(cc.egui_ctx.clone());
            let excluded_text = shared::shared().config.excluded_children.join(", ");
            Ok(Box::new(WinDozeApp { tray: build_tray(), excluded_text, pending_kill: None }))
        }),
    )
}

fn build_tray() -> Option<TrayIcon> {
    let open = MenuItem::new("Open WinDoze", true, None);
    let thaw_all = MenuItem::new("Wake all apps now", true, None);
    let pause = MenuItem::new("Pause / resume auto-doze", true, None);
    let quit_item = MenuItem::new("Quit (wakes everything)", true, None);
    let menu = Menu::new();
    let _ = menu.append(&open);
    let _ = menu.append(&thaw_all);
    let _ = menu.append(&pause);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_item);

    let (open_id, thaw_id, pause_id, quit_id) =
        (open.id().clone(), thaw_all.id().clone(), pause.id().clone(), quit_item.id().clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            request_show();
        } else if event.id == thaw_id {
            shared::send(Command::ThawAll);
        } else if event.id == pause_id {
            let paused = shared::shared().config.paused;
            shared::set_paused(!paused);
        } else if event.id == quit_id {
            quit();
        }
    }));
    TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            request_show();
        }
    }));

    // The icon embedded in the exe (crisp at every DPI); raw RGBA as a fallback.
    let icon = tray_icon::Icon::from_resource(1, None).or_else(|_| {
        let (rgba, size) = icon_rgba(32);
        tray_icon::Icon::from_rgba(rgba, size, size)
    });
    let icon = icon.ok()?;
    match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("WinDoze")
        .with_icon(icon)
        .build()
    {
        Ok(t) => Some(t),
        Err(e) => {
            logln!("tray icon failed: {e}");
            None
        }
    }
}

struct WinDozeApp {
    tray: Option<TrayIcon>,
    excluded_text: String,
    /// App waiting for "are you sure?" before force quitting: (exe, display name).
    pending_kill: Option<(String, String)>,
}

impl eframe::App for WinDozeApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if SHOW_REQUESTED.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_secs(1));

        let (original, status) = {
            let s = shared::shared();
            (s.config.clone(), s.status.clone())
        };
        let mut cfg = original.clone();
        let mut commands: Vec<Command> = Vec::new();

        if let Some(tray) = &self.tray {
            let tip = format!(
                "WinDoze: {} dozing, {} released. {} RAM available",
                status.frozen_count,
                fmt_bytes(status.total_saved_bytes),
                fmt_bytes(status.system_available)
            );
            let _ = tray.set_tooltip(Some(tip));
        }

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("WinDoze").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Wake all").clicked() {
                        commands.push(Command::ThawAll);
                    }
                    let can_release = status.frozen_count > 0;
                    if ui
                        .add_enabled(can_release, egui::Button::new("Free memory now"))
                        .on_hover_text(
                            "Push the memory of every dozing app out of RAM right now, even if RAM isn't low. \
                             Watch \"RAM available\" (or Task Manager) over the next few seconds to see the real effect.",
                        )
                        .clicked()
                    {
                        commands.push(Command::ReleaseNow);
                    }
                    let label = if cfg.paused { "Resume auto-doze" } else { "Pause auto-doze" };
                    if ui.button(label).clicked() {
                        cfg.paused = !cfg.paused;
                    }
                });
            });
            let state = if cfg.paused {
                RichText::new("Paused: nothing will be dozed").color(AMBER)
            } else {
                RichText::new("Running").color(Color32::from_rgb(34, 197, 94))
            };
            ui.horizontal(|ui| {
                ui.label(state);
                ui.label(format!(
                    "·  {} app(s) dozing  ·  {} released from dozing apps",
                    status.frozen_count,
                    fmt_bytes(status.total_saved_bytes)
                ))
                .on_hover_text(
                    "Private memory pushed out of dozing apps into Windows' compressed memory / pagefile. \
                     Watch \"RAM available\" below: that's Windows' own figure for what your other apps can use.",
                );
            });
            if status.system_total > 0 {
                let (note, color) = if status.memory_low {
                    ("low: dozing apps' memory is being released", AMBER)
                } else {
                    ("plenty free: dozing apps keep their memory so they wake instantly", Color32::GRAY)
                };
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "RAM available: {} of {}",
                        fmt_bytes(status.system_available),
                        fmt_bytes(status.system_total)
                    ));
                    ui.label(RichText::new(format!("({note})")).color(color).small());
                });
            }
            ui.add_space(6.0);
        });

        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(
                        "Panic key: Ctrl+Alt+Shift+T wakes everything and pauses.  Closing this window keeps WinDoze in the tray.",
                    )
                    .small()
                    .weak(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Quit").clicked() {
                        quit();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Apps you doze");
                ui.label(RichText::new("Only the apps you add here are ever put to sleep. Switching back to a dozing app wakes it.").weak());
                ui.add_space(4.0);
                // Only apps that are open right now get a card. Closed apps keep
                // their settings and come back automatically when reopened.
                let is_running = |exe: &str| {
                    !matches!(status.rules.get(exe).map(|s| &s.state), Some(RuleState::NotRunning))
                };
                let running_count = cfg.rules.iter().filter(|r| is_running(&r.exe)).count();
                if cfg.rules.is_empty() {
                    ui.label(RichText::new("No apps yet. Add one from \"Running apps\" below.").italics());
                } else if running_count == 0 {
                    ui.label(RichText::new("None of your apps are open right now.").italics());
                }
                let mut remove: Option<String> = None;
                for rule in cfg.rules.iter_mut().filter(|r| is_running(&r.exe)) {
                    let st = status.rules.get(&rule.exe);
                    rule_card(ui, rule, st, &mut commands, &mut remove, &mut self.pending_kill);
                    ui.add_space(6.0);
                }
                let closed: Vec<(String, String, Mode, u32)> = cfg
                    .rules
                    .iter()
                    .filter(|r| !is_running(&r.exe))
                    .map(|r| (r.exe.clone(), r.display_name.clone(), r.mode, r.minutes))
                    .collect();
                if !closed.is_empty() {
                    egui::CollapsingHeader::new(format!("Not running ({})", closed.len()))
                        .id_salt("not-running")
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("These apps are closed. Their settings are kept and apply again when you open them.")
                                    .small()
                                    .weak(),
                            );
                            for (exe, name, mode, minutes) in &closed {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(name).strong());
                                    ui.label(
                                        RichText::new(format!("{} after {minutes} min", mode.label().to_lowercase()))
                                            .weak()
                                            .small(),
                                    );
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button("Remove").clicked() {
                                            remove = Some(exe.clone());
                                        }
                                    });
                                });
                            }
                        });
                }
                if let Some(exe) = remove {
                    commands.push(Command::Thaw(exe.clone()));
                    cfg.rules.retain(|r| r.exe != exe);
                }

                ui.add_space(10.0);
                ui.separator();
                ui.heading("Running apps");
                ui.label(RichText::new("Apps with an open window, largest first. RAM includes all helper processes.").weak());
                ui.add_space(4.0);
                apps_table(ui, &status.apps, &mut cfg);

                ui.add_space(10.0);
                ui.separator();
                egui::CollapsingHeader::new("Settings").show(ui, |ui| {
                    settings(ui, &mut cfg, &mut self.excluded_text);
                });
            });
        });

        if let Some((exe, name)) = self.pending_kill.clone() {
            let modal = egui::Modal::new(egui::Id::new("confirm-force-quit")).show(ui.ctx(), |ui| {
                ui.set_width(360.0);
                ui.heading(format!("Force quit {name}?"));
                ui.add_space(4.0);
                ui.label("This closes all of its windows and processes right away. Anything you haven't saved will be lost.");
                ui.add_space(10.0);
                let mut done = false;
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Force quit").color(RED).strong()).clicked() {
                        commands.push(Command::Kill(exe.clone()));
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
                done
            });
            if modal.inner || modal.should_close() {
                self.pending_kill = None;
            }
        }

        // Apply changes.
        for rule in &cfg.rules {
            if let Some(old) = original.rule(&rule.exe)
                && old.enabled && !rule.enabled {
                    commands.push(Command::Thaw(rule.exe.clone()));
                }
        }
        if cfg.paused != original.paused {
            shared::set_paused(cfg.paused);
        }
        if cfg.start_with_windows != original.start_with_windows {
            crate::win::set_start_with_windows(cfg.start_with_windows);
        }
        if cfg != original {
            shared::shared().config = cfg.clone();
            config::save(&cfg);
            crate::engine::wake();
        }
        for c in commands {
            shared::send(c);
        }
    }
}

fn rule_card(
    ui: &mut egui::Ui,
    rule: &mut Rule,
    st: Option<&RuleStatus>,
    commands: &mut Vec<Command>,
    remove: &mut Option<String>,
    pending_kill: &mut Option<(String, String)>,
) {
    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let frozen = matches!(st.map(|s| &s.state), Some(RuleState::Frozen { .. }));
        let running = !matches!(st.map(|s| &s.state), None | Some(RuleState::NotRunning));

        ui.horizontal(|ui| {
            ui.checkbox(&mut rule.enabled, "");
            ui.label(RichText::new(&rule.display_name).strong().size(16.0));
            ui.label(RichText::new(&rule.exe).weak().small());
            if ui
                .small_button(RichText::new("Force quit").color(RED))
                .on_hover_text("Close the app right away, e.g. if it's stuck. Unsaved work is lost.")
                .clicked()
            {
                *pending_kill = Some((rule.exe.clone(), rule.display_name.clone()));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, color) = state_text(st);
                ui.label(RichText::new(text).color(color).strong());
            });
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("Doze");
            egui::ComboBox::from_id_salt(format!("mode-{}", rule.exe))
                .selected_text(rule.mode.label())
                .width(150.0)
                .show_ui(ui, |ui| {
                    for m in Mode::ALL {
                        ui.selectable_value(&mut rule.mode, m, m.label());
                    }
                });
            ui.label("after");
            ui.add(egui::DragValue::new(&mut rule.minutes).range(0..=240).suffix(" min"));
            ui.checkbox(&mut rule.trim, "Free its memory when RAM is low").on_hover_text(
                "While it dozes and RAM is running low, push its memory out of RAM (into Windows' compressed memory / pagefile) so your other apps can use it. \
                 With plenty of RAM free its memory stays put, so it wakes instantly.",
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Remove").clicked() {
                    *remove = Some(rule.exe.clone());
                }
                if frozen {
                    if ui.button("Wake").clicked() {
                        commands.push(Command::Thaw(rule.exe.clone()));
                    }
                } else if ui.add_enabled(running, egui::Button::new("Doze now")).clicked() {
                    commands.push(Command::FreezeNow(rule.exe.clone()));
                }
            });
        });

        let delay = if rule.minutes == 0 { "0 min = after 5 seconds. ".to_string() } else { String::new() };
        ui.label(RichText::new(format!("{}{}", delay, rule.mode.help())).small().weak());
        if let Some(s) = st
            && running {
                ui.label(
                    RichText::new(format!(
                        "{} process(es) · {} RAM · {:.1}% CPU",
                        s.procs,
                        fmt_bytes(s.mem_bytes),
                        s.cpu_percent
                    ))
                    .small(),
                );
            }
    });
}

fn state_text(st: Option<&RuleStatus>) -> (String, Color32) {
    let gray = Color32::GRAY;
    match st.map(|s| &s.state) {
        None => ("…".into(), gray),
        Some(RuleState::NotRunning) => ("Not running".into(), gray),
        Some(RuleState::Disabled) => ("Off".into(), gray),
        Some(RuleState::Paused) => ("Paused".into(), AMBER),
        Some(RuleState::Waiting(why)) => (format!("Active: {why}"), gray),
        Some(RuleState::Counting(left)) => (format!("Dozing in {}", fmt_duration(*left)), AMBER),
        Some(RuleState::Frozen { for_secs, saved_bytes, trimmed }) => {
            let memory = if *trimmed {
                format!(", {} released", fmt_bytes(*saved_bytes))
            } else {
                ", memory kept".to_string()
            };
            (format!("Dozing {}{}", fmt_duration(Duration::from_secs(*for_secs)), memory), FROZEN_BLUE)
        }
        Some(RuleState::Error(e)) => (e.clone(), RED),
    }
}

fn apps_table(ui: &mut egui::Ui, apps: &[AppInfo], cfg: &mut Config) {
    if apps.is_empty() {
        ui.label(RichText::new("Scanning…").weak());
        return;
    }
    egui::Grid::new("apps").striped(true).num_columns(5).spacing([16.0, 6.0]).show(ui, |ui| {
        ui.label(RichText::new("App").strong());
        ui.label(RichText::new("Window").strong());
        ui.label(RichText::new("Processes").strong());
        ui.label(RichText::new("RAM").strong());
        ui.label("");
        ui.end_row();
        for app in apps {
            ui.label(&app.name).on_hover_text(&app.exe);
            let mut title = app.title.clone();
            if title.chars().count() > 40 {
                title = title.chars().take(40).collect::<String>() + "…";
            }
            ui.label(RichText::new(title).weak());
            ui.label(app.procs.to_string());
            ui.label(fmt_bytes(app.mem_bytes));
            let added = cfg.rule(&app.exe).is_some();
            if added {
                ui.label(RichText::new("Added").weak());
            } else if ui.button("Add").clicked() {
                cfg.rules.push(Rule::new(&app.exe, &app.name));
            }
            ui.end_row();
        }
    });
}

fn settings(ui: &mut egui::Ui, cfg: &mut Config, excluded_text: &mut String) {
    ui.horizontal(|ui| {
        ui.label("\"Idle\" means using less than");
        ui.add(egui::Slider::new(&mut cfg.idle_cpu_percent, 0.1..=10.0).suffix("% of total CPU"));
    });
    ui.checkbox(&mut cfg.skip_if_playing_audio, "Never doze an app that is playing audio (music, calls, videos)");
    ui.checkbox(
        &mut cfg.skip_if_clipboard_owner,
        "Keep the app you just copied from awake for a minute, so pasting from it works",
    );
    ui.checkbox(&mut cfg.start_with_windows, "Start WinDoze with Windows (in the tray)");
    ui.add_space(6.0);
    ui.label("Never doze these child processes, or anything they start. This keeps builds and dev servers in an editor's terminal alive, but one that prints a lot of output may still pause until the editor is woken, because the editor is what reads that output:");
    let resp = ui.add(egui::TextEdit::multiline(excluded_text).desired_rows(2).desired_width(f32::INFINITY));
    if resp.changed() {
        cfg.excluded_children = excluded_text
            .split([',', '\n', ' '])
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
    }
    ui.add_space(6.0);
    if ui.button("Open log folder").clicked() {
        let _ = std::process::Command::new("explorer").arg(config::data_dir()).spawn();
    }
}

// ---------------------------------------------------------------------------

fn fmt_bytes(b: u64) -> String {
    let mb = b as f64 / 1_048_576.0;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// 😴 from Noto Emoji (Apache 2.0), pre-converted to raw RGBA by size.
const ICON_64: &[u8] = include_bytes!("../assets/icon-64.rgba");
const ICON_32: &[u8] = include_bytes!("../assets/icon-32.rgba");

fn icon_rgba(size: u32) -> (Vec<u8>, u32) {
    match size {
        32 => (ICON_32.to_vec(), 32),
        _ => (ICON_64.to_vec(), 64),
    }
}
