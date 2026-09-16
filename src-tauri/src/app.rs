//! egui UI: immediate-mode port of the old Solid frontend. The GUI thread is
//! purely presentational: it renders the latest [`FrameState`] pushed by the
//! work thread and sends [`UiIntent`]s back. No `Arc`, no `Mutex` here.

#[cfg(windows)]
use windows_sys::Win32::{Foundation::HWND, UI::WindowsAndMessaging as wam};

use crossbeam_channel::{Receiver, Sender};

use crate::hermes::{StatusEvent, SubStatus};
use crate::state::{FrameState, UiIntent};
use crate::tray::{Tray, TrayAction};

const GREEN: egui::Color32 = egui::Color32::from_rgb(46, 160, 67);
const RED: egui::Color32 = egui::Color32::from_rgb(218, 54, 51);
const GRAY: egui::Color32 = egui::Color32::from_rgb(110, 118, 129);

pub struct SiphonApp {
    ui_tx: Sender<UiIntent>,
    frame_rx: Receiver<FrameState>,
    tray_rx: Receiver<TrayAction>,
    frame: FrameState,
    /// `None` when the tray icon failed to build: closing the window quits
    /// instead of hiding, so it can never strand invisible without a tray.
    /// Also owns the icon; dropping removes it from the tray.
    tray: Option<Tray>,
    single: app_single_instance::PrimaryHandle,
    #[cfg_attr(not(windows), allow(dead_code))]
    hwnd: Option<isize>,
    new_login: String,
    quit_requested: bool,
}

impl SiphonApp {
    // Takes owning channel ends and guards; cannot be `const`.
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(
        ui_tx: Sender<UiIntent>,
        frame_rx: Receiver<FrameState>,
        tray_rx: Receiver<TrayAction>,
        initial: FrameState,
        tray: Option<Tray>,
        single: app_single_instance::PrimaryHandle,
        hwnd: Option<isize>,
    ) -> Self {
        Self {
            ui_tx,
            frame_rx,
            tray_rx,
            frame: initial,
            tray,
            single,
            hwnd,
            new_login: String::new(),
            quit_requested: false,
        }
    }

    /// Unhide + focus the window: Win32 `ShowWindow` on Windows (the
    /// cross-platform viewport commands don't reliably re-show a hidden
    /// winit window, see emilk/egui#737), viewport commands elsewhere.
    fn show(&self, ctx: &egui::Context) {
        log::info!(target: "tray", "show requested (hwnd={:?})", self.hwnd);
        #[cfg(windows)]
        if let Some(hwnd) = self.hwnd {
            set_visible(hwnd, wam::SW_SHOWDEFAULT);
            // SAFETY: our own live window; a foreground request needs
            // nothing beyond a valid handle.
            unsafe {
                wam::SetForegroundWindow(hwnd as HWND);
            }
        }
        #[cfg(not(windows))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Closing the window keeps the app running in the tray. Without a
    /// working tray the window would strand invisible, so fall through to
    /// a real quit instead.
    fn hide(&mut self, ctx: &egui::Context) {
        log::info!(target: "tray", "hide requested (hwnd={:?})", self.hwnd);
        if self.tray.is_none() {
            self.quit_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        #[cfg(windows)]
        if let Some(hwnd) = self.hwnd {
            set_visible(hwnd, wam::SW_HIDE);
        }
        #[cfg(not(windows))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn submit_add(&mut self) {
        let login = self.new_login.trim().to_owned();
        self.new_login.clear();
        if login.is_empty() {
            return;
        }
        // Fire and forget: the work thread validates, resolves, persists,
        // and pushes a fresh frame back. The receiver lives as long as the
        // work thread, so a send failure only means shutdown.
        let _ = self.ui_tx.send(UiIntent::AddLogin(login));
    }

    fn render(&mut self, ui: &mut egui::Ui) {
        // Owned per-frame copies (as the old snapshots were): the `show`
        // closure below calls `&mut self` methods, so it cannot borrow
        // `self.frame` at the same time.
        let config = self.frame.config.clone();
        let status = self.frame.status.clone();
        let error = self.frame.error.clone();
        let status = status.as_ref();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Siphon");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = connection_text(status);
                    ui.colored_label(color, text);
                });
            });
            ui.separator();

            egui::Grid::new("channels")
                .num_columns(4)
                .spacing([8.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label("");
                    ui.label("Live Status");
                    ui.label("Title Status");
                    ui.label("");
                    ui.end_row();
                    for channel in &config.channels {
                        let resolved = status.and_then(|snapshot| {
                            snapshot
                                .channels
                                .iter()
                                .find(|entry| entry.channel_id == channel.id)
                        });
                        let not_found = status.is_some_and(|snapshot| {
                            snapshot
                                .unresolved
                                .iter()
                                .any(|login| login.eq_ignore_ascii_case(&channel.login))
                        });
                        let name = resolved
                            .map(|entry| entry.display_name.clone())
                            .or_else(|| channel.display_name.clone())
                            .unwrap_or_else(|| channel.login.clone());
                        if not_found {
                            ui.colored_label(RED, name);
                            badge(ui, "Not found", RED);
                            badge(ui, "Not found", RED);
                        } else {
                            ui.label(name);
                            sub_badge(
                                ui,
                                resolved.map_or(SubStatus::Pending, |entry| entry.live_status),
                            );
                            sub_badge(
                                ui,
                                resolved.map_or(SubStatus::Pending, |entry| entry.title_status),
                            );
                        }
                        if ui
                            .button("×")
                            .on_hover_text(format!("Remove {}", channel.login))
                            .clicked()
                        {
                            let _ = self
                                .ui_tx
                                .send(UiIntent::RemoveLogin(channel.login.clone()));
                        }
                        ui.end_row();
                    }
                });
            if config.channels.is_empty() {
                ui.label("No channels configured. Add a streamer below.");
            }

            ui.separator();
            ui.horizontal(|ui| {
                let response = ui
                    .text_edit_singleline(&mut self.new_login)
                    .on_hover_text("Add new streamer");
                let submitted = ui.button("Add").clicked()
                    || (response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                if submitted {
                    self.submit_add();
                }
            });

            let mut notify = config.notify_title_changes;
            if ui
                .checkbox(&mut notify, "Notify on title changes while offline")
                .changed()
            {
                let _ = self.ui_tx.send(UiIntent::SetNotifyTitleChanges(notify));
            }
            let mut sound = config.sound;
            if ui.checkbox(&mut sound, "Notification sound").changed() {
                let _ = self.ui_tx.send(UiIntent::SetSound(sound));
            }

            if !error.is_empty() {
                ui.colored_label(RED, error);
            }
        });
    }
}

impl eframe::App for SiphonApp {
    /// Non-painting work. Runs before every `ui`, and keeps running while
    /// the window is hidden — which is exactly when tray/single-instance
    /// events need handling.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Second launch wakes us instead of starting a new copy.
        if self.single.check_show() {
            log::info!(target: "single", "wake received, showing window");
            self.show(ctx);
        }
        // Drain to latest action is unnecessary here: each action is a real
        // user gesture, so handle every queued one in order.
        while let Ok(action) = self.tray_rx.try_recv() {
            match action {
                TrayAction::Show => {
                    log::info!(target: "tray", "tray Open/left-click, showing window");
                    self.show(ctx);
                }
                TrayAction::Quit => {
                    self.quit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        // Drain to latest: intermediate frames are stale the moment a newer
        // push lands, so only the last one matters.
        if let Some(frame) = self.frame_rx.try_iter().last() {
            self.frame = frame;
        }
        if ctx.input(|input| input.viewport().close_requested()) && !self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render(ui);
    }
}

fn connection_text(status: Option<&StatusEvent>) -> (String, egui::Color32) {
    match status {
        Some(snapshot) if snapshot.connected => ("Connected".to_owned(), GREEN),
        Some(snapshot) => (
            snapshot
                .error
                .clone()
                .unwrap_or_else(|| "Connecting…".to_owned()),
            RED,
        ),
        None => ("Connecting…".to_owned(), GRAY),
    }
}

fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.colored_label(color, text);
}

fn sub_badge(ui: &mut egui::Ui, state: SubStatus) {
    let (text, color) = match state {
        SubStatus::Pending => ("Pending", GRAY),
        SubStatus::Connected => ("Connected", GREEN),
        SubStatus::Failed => ("Failed", RED),
    };
    badge(ui, text, color);
}

/// Visibility toggle for our own window. `ViewportCommand::Visible` hides
/// fine but doesn't reliably re-show (emilk/egui#737), hence Win32 here.
#[cfg(windows)]
fn set_visible(hwnd: isize, cmd: i32) {
    // SAFETY: `hwnd` is the live eframe window captured at startup;
    // `ShowWindow` only toggles visibility.
    unsafe {
        wam::ShowWindow(hwnd as HWND, cmd);
    }
}
