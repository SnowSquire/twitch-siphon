//! egui UI. The GUI thread is
//! purely presentational: it renders the latest [`FrameState`] pushed by the
//! work thread and sends [`UiIntent`]s back. No `Arc`, no `Mutex` here.

#[cfg(windows)]
use windows_sys::Win32::{Foundation::HWND, UI::WindowsAndMessaging as wam};

use kanal::{Receiver, Sender};

use crate::hermes::{StatusSnapshot, SubStatus};
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
    /// Zoom last synced from the framework. A pass that applies a zoom
    /// change reports a rescaled input rect, so measurement is only valid
    /// on passes where this matches the live factor.
    last_zoom: f32,
    /// Text column widths in points. Point sizes are independent of the
    /// zoom factor, so one measurement serves every frame.
    col_widths: Option<(f32, f32, f32)>,
}

impl SiphonApp {
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
            last_zoom: 1.0,
            col_widths: None,
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

    fn submit_add(new_login: &mut String, ui_tx: &Sender<UiIntent>) {
        let login = new_login.trim().to_owned();
        new_login.clear();
        if login.is_empty() {
            return;
        }
        // Fire and forget: the work thread validates, resolves, persists,
        // and pushes a fresh frame back. The receiver lives as long as the
        // work thread, so a send failure only means shutdown.
        let _ = ui_tx.send(UiIntent::AddLogin(login));
    }

    fn render(&mut self, ui: &mut egui::Ui) {
        // Measured once: six shapings plus the exclusive font lock on
        // every frame shows up directly in resize latency.
        if self.col_widths.is_none() {
            let button_pad_x = ui.spacing().button_padding.x;
            self.col_widths = Some(ui.ctx().fonts_mut(|fonts| {
                let mut measure = |text: &str| {
                    fonts
                        .layout_no_wrap(
                            text.to_owned(),
                            egui::FontId::default(),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x
                };
                let badges = ["Connected", "Pending", "Failed"]
                    .iter()
                    .map(|text| measure(text))
                    .fold(0.0_f32, f32::max);
                let live = measure("Live Status").max(badges) + 4.0;
                let title = measure("Title Status").max(badges) + 4.0;
                let remove = measure("×") + button_pad_x * 2.0 + 8.0;
                (live, title, remove)
            }));
        }
        // Disjoint field borrows let the panels mutate the input line and
        // send intents while reading the latest frame, with no per-frame
        // clones of config, status, or error.
        let frame = &self.frame;
        let ui_tx = &self.ui_tx;
        let new_login = &mut self.new_login;
        let config = &frame.config;
        let status = frame.status.as_ref();
        let error = frame.error.as_str();
        let pending_guard = status.and_then(|snapshot| snapshot.pending_adds.read().ok());
        let pending_adds: &[String] =
            pending_guard.as_deref().map_or(&[], Vec::as_slice);

        // Pinned below the list so a long list scrolls instead of pushing
        // the input and toggles off-screen.
        egui::Panel::bottom("controls").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                // Equal heights keep the row aligned as the window resizes.
                let height = ui.spacing().interact_size.y;
                let spacing = ui.spacing().item_spacing.x;
                let button_width = 44.0;
                let text_width = (ui.available_width() - button_width - spacing).max(60.0);
                let response = ui
                    .add_sized(
                        [text_width, height],
                        egui::TextEdit::singleline(&mut *new_login)
                            .hint_text("Add new streamer")
                            .vertical_align(egui::Align::Center),
                    )
                    .on_hover_text("Add new streamer");
                let submitted = ui
                    .add_sized([button_width, height], egui::Button::new("Add"))
                    .clicked()
                    || (response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                if submitted {
                    Self::submit_add(new_login, ui_tx);
                }
            });

            let mut notify = config.notify_title_changes;
            if ui
                .checkbox(&mut notify, "Notify on title changes while offline")
                .changed()
            {
                let _ = ui_tx.send(UiIntent::SetNotifyTitleChanges(notify));
            }
            let mut sound = config.sound;
            if ui.checkbox(&mut sound, "Notification sound").changed() {
                let _ = ui_tx.send(UiIntent::SetSound(sound));
            }

            if !error.is_empty() {
                ui.horizontal(|ui| {
                    ui.colored_label(RED, error);
                    if ui.button("×").on_hover_text("Dismiss").clicked() {
                        let _ = ui_tx.send(UiIntent::ClearError);
                    }
                });
            }
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Siphon");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = connection_text(status);
                    ui.colored_label(color, text);
                });
            });
            ui.separator();

            // Right columns keep cached text widths so each row sums to the
            // available width. Rows are plain horizontal strips: every
            // position comes from current-frame sizes, so nothing lags a
            // resize by a frame.
            let row_height = ui.spacing().interact_size.y;
            let col_spacing = ui.spacing().item_spacing.x;
            let stripe = ui.visuals().faint_bg_color;
            let (live_width, title_width, remove_width) =
                self.col_widths.unwrap_or((80.0, 80.0, 32.0));
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Measured inside the scroll area so a visible scrollbar
                    // is already subtracted from the available width.
                    let name_width = (ui.available_width()
                        - live_width
                        - title_width
                        - remove_width
                        - col_spacing * 3.0)
                        .max(40.0);
                    // Header is row zero, which carries no stripe.
                    table_row(ui, false, stripe, row_height, |ui| {
                        ui.add_sized(
                            [name_width, row_height],
                            egui::Label::new("Channel")
                                .halign(egui::Align::LEFT)
                                .selectable(false),
                        );
                        ui.add_sized(
                            [live_width, row_height],
                            egui::Label::new("Live Status").selectable(false),
                        );
                        ui.add_sized(
                            [title_width, row_height],
                            egui::Label::new("Title Status").selectable(false),
                        );
                        ui.allocate_space(egui::vec2(remove_width, row_height));
                    });
                    let mut striped = true;
                    for channel in &config.channels {
                        let resolved = status.and_then(|snapshot| {
                            snapshot
                                .channels
                                .iter()
                                .find(|entry| entry.channel_id == channel.id)
                        });
                        let name = resolved
                            .map(|entry| entry.display_name.as_str())
                            .or(channel.display_name.as_deref())
                            .unwrap_or(channel.login.as_str());
                        table_row(ui, striped, stripe, row_height, |ui| {
                            ui.add_sized(
                                [name_width, row_height],
                                egui::Label::new(name)
                                    .halign(egui::Align::LEFT)
                                    .selectable(false)
                                    .truncate(),
                            );
                            sub_badge(
                                ui,
                                egui::vec2(live_width, row_height),
                                resolved.map_or(SubStatus::Pending, |entry| entry.live_status),
                            );
                            sub_badge(
                                ui,
                                egui::vec2(title_width, row_height),
                                resolved.map_or(SubStatus::Pending, |entry| entry.title_status),
                            );
                            let remove = ui.add_sized(
                                [remove_width, row_height],
                                egui::Button::new("×"),
                            );
                            // Formatted only while hovered: idle rows skip
                            // the allocation behind the tooltip.
                            let remove = if remove.hovered() {
                                remove.on_hover_text(format!("Remove {}", channel.login))
                            } else {
                                remove
                            };
                            if remove.clicked() {
                                let _ = ui_tx.send(UiIntent::RemoveChannel(channel.id));
                            }
                        });
                        striped = !striped;
                    }
                    for login in pending_adds {
                        table_row(ui, striped, stripe, row_height, |ui| {
                            ui.add_sized(
                                [name_width, row_height],
                                egui::Label::new(login.as_str())
                                    .halign(egui::Align::LEFT)
                                    .selectable(false)
                                    .truncate(),
                            );
                            sub_badge(ui, egui::vec2(live_width, row_height), SubStatus::Pending);
                            sub_badge(ui, egui::vec2(title_width, row_height), SubStatus::Pending);
                            ui.add_sized(
                                [remove_width, row_height],
                                egui::Label::new("…").selectable(false),
                            );
                        });
                        striped = !striped;
                    }
                });
            if config.channels.is_empty() && pending_adds.is_empty() {
                ui.label("No channels configured. Add a streamer below.");
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
        // Every queued action is a user gesture; handle each in order.
        while let Ok(Some(action)) = self.tray_rx.try_recv() {
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
        let mut latest = None;
        while let Ok(Some(frame)) = self.frame_rx.try_recv() {
            latest = Some(frame);
        }
        if let Some(frame) = latest {
            self.frame = frame;
        }
        if ctx.input(|input| input.viewport().close_requested()) && !self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Scale text with window width off the 420pt design width with a
        // 1.25x base so type stays readable at the minimum size. The input
        // rect is divided by the live zoom, so the factor is recovered by
        // multiplying back; the pass that applies a change still reports
        // a rescaled rect, which is skipped via `last_zoom`.
        let current = ui.ctx().zoom_factor();
        if current != self.last_zoom {
            self.last_zoom = current;
        } else if let Some(rect) = ui.ctx().input(|input| input.viewport().inner_rect) {
            let scale = (rect.width() * current / 420.0 * 1.25).clamp(1.25, 5.0);
            if (scale - current).abs() > 0.001 {
                ui.ctx().set_zoom_factor(scale);
            }
        }
        self.render(ui);
    }
}

fn connection_text(status: Option<&StatusSnapshot>) -> (&str, egui::Color32) {
    match status {
        Some(snapshot) if snapshot.connected => ("Connected", GREEN),
        Some(snapshot) => (
            snapshot.error.as_deref().unwrap_or("Connecting…"),
            RED,
        ),
        None => ("Connecting…", GRAY),
    }
}

fn table_row(
    ui: &mut egui::Ui,
    striped: bool,
    stripe: egui::Color32,
    height: f32,
    add_cells: impl FnOnce(&mut egui::Ui),
) {
    if striped {
        // Painted up front from a known height so the stripe matches the
        // row it backs.
        let top = ui.cursor().min;
        let width = ui.available_width();
        ui.painter().rect_filled(
            egui::Rect::from_min_size(top, egui::vec2(width, height)),
            2.0,
            stripe,
        );
    }
    ui.horizontal(add_cells);
}

fn badge(ui: &mut egui::Ui, size: egui::Vec2, text: &str, color: egui::Color32) {
    ui.add_sized(
        size,
        egui::Label::new(egui::RichText::new(text).color(color))
            .halign(egui::Align::LEFT)
            .selectable(false),
    );
}

fn sub_badge(ui: &mut egui::Ui, size: egui::Vec2, state: SubStatus) {
    let (text, color) = match state {
        SubStatus::Pending => ("Pending", GRAY),
        SubStatus::Connected => ("Connected", GREEN),
        SubStatus::Failed => ("Failed", RED),
    };
    badge(ui, size, text, color);
}

/// Visibility toggle for our own window.
#[cfg(windows)]
fn set_visible(hwnd: isize, cmd: i32) {
    // SAFETY: `hwnd` is the live eframe window captured at startup;
    // `ShowWindow` only toggles visibility.
    unsafe {
        wam::ShowWindow(hwnd as HWND, cmd);
    }
}
