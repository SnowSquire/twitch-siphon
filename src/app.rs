#[cfg(windows)]
use windows_sys::Win32::{Foundation::HWND, UI::WindowsAndMessaging as wam};

use std::time::{Duration, Instant};

use kanal::{Receiver, Sender};

use crate::hermes::{StatusSnapshot, SubStatus};
use crate::state::{FrameState, SharedFrame, UiIntent, UpdateStatus};
use crate::tray::{Tray, TrayAction};

const GREEN: egui::Color32 = egui::Color32::from_rgb(46, 160, 67);
const RED: egui::Color32 = egui::Color32::from_rgb(218, 54, 51);
const GRAY: egui::Color32 = egui::Color32::from_rgb(110, 118, 129);

const DESIGN_WIDTH: f32 = 420.0;
const BASE_SCALE: f32 = 1.25;
const MIN_SCALE: f32 = 1.25;
const MAX_SCALE: f32 = 5.0;
const SCALE_DELTA: f32 = 0.01;
const SCALE_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Channels,
    FilteredWords,
}

pub struct SiphonApp {
    ui_tx: Sender<UiIntent>,
    shared: SharedFrame,
    shared_version: u64,
    tray_rx: Receiver<TrayAction>,
    frame: FrameState,
    /// `None` when the tray icon failed to build: closing the window quits
    /// instead of hiding, so it can never strand invisible without a tray.
    /// Also owns the icon; dropping removes it from the tray.
    tray: Option<Tray>,
    single: app_single_instance::PrimaryHandle,
    #[cfg(windows)]
    hwnd: isize,
    new_login: String,
    new_filtered_word: String,
    tab: Tab,
    quit_requested: bool,
    /// Text column widths in points.
    col_widths: Option<(f32, f32, f32)>,
    last_scale: Option<Instant>,
}

impl SiphonApp {
    pub fn new(
        ui_tx: Sender<UiIntent>,
        shared: SharedFrame,
        tray_rx: Receiver<TrayAction>,
        tray: Option<Tray>,
        single: app_single_instance::PrimaryHandle,
        #[cfg(windows)] hwnd: isize,
    ) -> Self {
        let (frame, shared_version) = shared.load();
        Self {
            ui_tx,
            shared,
            shared_version,
            tray_rx,
            frame,
            tray,
            single,
            #[cfg(windows)]
            hwnd,
            new_login: String::new(),
            new_filtered_word: String::new(),
            tab: Tab::Channels,
            quit_requested: false,
            col_widths: None,
        }
    }

    /// Unhide + focus the window: Win32 `ShowWindow` on Windows (the
    /// cross-platform viewport commands don't reliably re-show a hidden
    /// winit window, see emilk/egui#737), viewport commands elsewhere.
    fn show(&self, ctx: &egui::Context) {
        #[cfg(windows)]
        log::info!(target: "tray", "show requested (hwnd={:?})", self.hwnd);
        #[cfg(not(windows))]
        log::info!(target: "tray", "show requested");
        #[cfg(windows)]
        {
            set_visible(self.hwnd, wam::SW_SHOWDEFAULT);
            // SAFETY: our own live window; a foreground request needs
            // nothing beyond a valid handle.
            unsafe {
                wam::SetForegroundWindow(self.hwnd as HWND);
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
        #[cfg(windows)]
        log::info!(target: "tray", "hide requested (hwnd={:?})", self.hwnd);
        #[cfg(not(windows))]
        log::info!(target: "tray", "hide requested");
        if self.tray.is_none() {
            self.quit_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        #[cfg(windows)]
        set_visible(self.hwnd, wam::SW_HIDE);

        #[cfg(not(windows))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn column_widths(&mut self, ui: &egui::Ui) -> (f32, f32, f32) {
        if let Some(widths) = self.col_widths {
            return widths;
        }
        // Measured once: six shapings plus the exclusive font lock on
        // every frame shows up directly in resize latency.
        let button_pad_x = ui.spacing().button_padding.x;
        let widths = ui.ctx().fonts_mut(|fonts| {
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
        });
        self.col_widths = Some(widths);
        widths
    }

    fn render(&mut self, ui: &mut egui::Ui) {
        let widths = self.column_widths(ui);
        // Disjoint field borrows let the panels mutate the input line and
        // send intents while reading the latest frame, with no per-frame
        // clones of config, status, or error.
        let pending_guard = self
            .frame
            .status
            .as_ref()
            .and_then(|snapshot| snapshot.pending_adds.read().ok());
        let pending_adds: &[String] = pending_guard.as_deref().map_or(&[], Vec::as_slice);

        // Pinned below the list so a long list scrolls instead of pushing
        // the input and toggles off-screen. The input follows the active
        // tab, so adding works wherever the user is looking.
        egui::Panel::bottom("controls").show(ui, |ui| {
            ui.add_space(8.0);
            // Equal heights keep the row aligned as the window resizes.
            let height = ui.spacing().interact_size.y;
            let spacing = ui.spacing().item_spacing.x;
            let button_width = 44.0;
            let text_width = (ui.available_width() - button_width - spacing).max(60.0);
            let (input, hint, intent): (&mut String, &str, fn(String) -> UiIntent) = match self.tab
            {
                Tab::Channels => (&mut self.new_login, "Add new streamer", UiIntent::AddLogin),
                Tab::FilteredWords => (
                    &mut self.new_filtered_word,
                    "Add filtered word",
                    UiIntent::AddFilteredWord,
                ),
            };
            ui.horizontal(|ui| {
                let response = ui
                    .add_sized(
                        [text_width, height],
                        egui::TextEdit::singleline(&mut *input)
                            .hint_text(hint)
                            .vertical_align(egui::Align::Center),
                    )
                    .on_hover_text(hint);
                let submitted = ui
                    .add_sized([button_width, height], egui::Button::new("Add"))
                    .clicked()
                    || (response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                if submitted {
                    let value = input.trim().to_owned();
                    input.clear();
                    if !value.is_empty() {
                        let _ = self.ui_tx.send(intent(value));
                    }
                }
            });

            let mut notify = self.frame.config.notify_title_changes;
            if toggled(ui, &mut notify, "Notify on title changes while offline") {
                let _ = self.ui_tx.send(UiIntent::SetNotifyTitleChanges(notify));
            }

            if toggled(ui, &mut self.frame.config.sound, "Notification sound") {
                let _ = self.ui_tx.send(UiIntent::SetSound(self.frame.config.sound));
            }

            if !self.frame.error.is_empty() {
                // Wrapped, not horizontal: long errors (download failures
                // carry urls and statuses) must wrap instead of running
                // off-screen, with the dismiss button flowing inline.
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(RED, self.frame.error.as_str());
                    if ui.button("×").on_hover_text("Dismiss").clicked() {
                        let _ = self.ui_tx.send(UiIntent::ClearError);
                    }
                });
            }
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                // Chrome, not content: never selectable, so clicks and
                // drags around the button never start a text selection.
                ui.add(egui::Label::new(egui::RichText::new("Siphon").heading()).selectable(false))
                    .on_hover_text(concat!("Siphon ", env!("CARGO_PKG_VERSION")));
                // The only upgrade path: toast clicks never install.
                match &self.frame.update {
                    UpdateStatus::Available(offer) => {
                        if ui
                            .button(format!("Update to {}", offer.version))
                            .on_hover_text("Download, install, and reopen")
                            .clicked()
                        {
                            let _ = self.ui_tx.send(UiIntent::ApplyUpdate);
                        }
                    }
                    UpdateStatus::Downloading(_) => {
                        ui.add_enabled(false, egui::Button::new("Updating…"));
                    }
                    UpdateStatus::Idle | UpdateStatus::Checking | UpdateStatus::Current => {}
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = connection_text(self.frame.status.as_ref());
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).color(color)).selectable(false),
                    );
                });
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Channels, "Channels");
                ui.selectable_value(&mut self.tab, Tab::FilteredWords, "Filtered words");
            });
            ui.separator();

            match self.tab {
                Tab::Channels => {
                    if self.frame.config.channels.is_empty() && pending_adds.is_empty() {
                        ui.label("No channels configured. Add a streamer below.");
                    } else {
                        // Right columns keep cached text widths so each row sums to the
                        // available width. Rows are plain horizontal strips: every
                        // position comes from current-frame sizes, so nothing lags a
                        // resize by a frame.
                        let row_height = ui.spacing().interact_size.y;
                        let col_spacing = ui.spacing().item_spacing.x;
                        let stripe = ui.visuals().faint_bg_color;
                        let (live_width, title_width, remove_width) = widths;
                        egui::ScrollArea::vertical()
                            .scroll_bar_visibility(
                                egui::containers::scroll_area::ScrollBarVisibility::AlwaysVisible,
                            )
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
                                    name_label(
                                        ui,
                                        egui::vec2(name_width, row_height),
                                        "Channel",
                                        false,
                                    );
                                    name_label(
                                        ui,
                                        egui::vec2(live_width, row_height),
                                        "Live Status",
                                        false,
                                    );
                                    name_label(
                                        ui,
                                        egui::vec2(title_width, row_height),
                                        "Title Status",
                                        false,
                                    );
                                    ui.allocate_space(egui::vec2(remove_width, row_height));
                                });
                                let mut striped = true;
                                let widths = (live_width, title_width, remove_width);
                                for channel in &self.frame.config.channels {
                                    let resolved =
                                        self.frame.status.as_ref().and_then(|snapshot| {
                                            snapshot
                                                .channels
                                                .iter()
                                                .find(|entry| entry.channel_id == channel.id)
                                        });
                                    let name = resolved
                                        .map(|entry| entry.display_name.as_str())
                                        .or(channel.display_name.as_deref())
                                        .unwrap_or(channel.login.as_str());
                                    channel_row(
                                        ui,
                                        RowLayout {
                                            striped,
                                            stripe,
                                            height: row_height,
                                            widths,
                                            name_width,
                                        },
                                        RowContent {
                                            name,
                                            live: resolved.map_or(SubStatus::Pending, |entry| {
                                                entry.live_status
                                            }),
                                            title: resolved.map_or(SubStatus::Pending, |entry| {
                                                entry.title_status
                                            }),
                                            remove: Some((channel.id, channel.login.as_str())),
                                        },
                                        &self.ui_tx,
                                    );
                                    striped = !striped;
                                }
                                for login in pending_adds {
                                    channel_row(
                                        ui,
                                        RowLayout {
                                            striped,
                                            stripe,
                                            height: row_height,
                                            widths,
                                            name_width,
                                        },
                                        RowContent {
                                            name: login.as_str(),
                                            live: SubStatus::Pending,
                                            title: SubStatus::Pending,
                                            remove: None,
                                        },
                                        &self.ui_tx,
                                    );
                                    striped = !striped;
                                }
                            });
                    }
                }
                Tab::FilteredWords => {
                    ui.label(
                        "Titles containing these words stay silent for title-change notifications.",
                    );
                    ui.horizontal_wrapped(|ui| {
                        for (index, word) in self.frame.config.filtered_words.iter().enumerate() {
                            let remove = ui.button(format!("{word} ×"));
                            // Formatted only while hovered: idle rows skip
                            // the allocation behind the tooltip.
                            let remove = if remove.hovered() {
                                remove.on_hover_text(format!("Remove {word}"))
                            } else {
                                remove
                            };
                            if remove.clicked() {
                                let _ = self.ui_tx.send(UiIntent::RemoveFilteredWord(index));
                            }
                        }
                    });
                    if self.frame.config.filtered_words.is_empty() {
                        ui.label("No filtered words. Add one below.");
                    }
                }
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
        // Copy out the latest snapshot when the slot moved on: intermediate
        // states are already dropped by the writer, so only the last one
        // matters.
        if let Some((frame, version)) = self.shared.load_newer(self.shared_version) {
            self.frame = frame;
            self.shared_version = version;
        }
        if ctx.input(|input| input.viewport().close_requested()) && !self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let scale = frame.winit_window().map(|window| {
            let width = window.inner_size().width as f32 / window.scale_factor() as f32;
            (width / DESIGN_WIDTH * BASE_SCALE).clamp(MIN_SCALE, MAX_SCALE)
        });
        if let Some(scale) = scale {
            ui.ctx().set_zoom_factor(scale);
        }
        self.render(ui);
    }
}

fn toggled(ui: &mut egui::Ui, value: &mut bool, label: &str) -> bool {
    ui.checkbox(value, label).changed()
}

fn connection_text(status: Option<&StatusSnapshot>) -> (&str, egui::Color32) {
    match status {
        Some(snapshot) if snapshot.connected => ("Connected", GREEN),
        Some(snapshot) => (snapshot.error.as_deref().unwrap_or("Connecting…"), RED),
        None => ("Connecting…", GRAY),
    }
}

fn name_label(ui: &mut egui::Ui, size: egui::Vec2, text: &str, selectable: bool) {
    let mut label = egui::Label::new(text)
        .halign(egui::Align::LEFT)
        .selectable(selectable);
    if selectable {
        label = label.truncate();
    }
    ui.add_sized(size, label);
}

#[derive(Clone, Copy)]
struct RowLayout {
    striped: bool,
    stripe: egui::Color32,
    height: f32,
    widths: (f32, f32, f32),
    name_width: f32,
}

#[derive(Clone, Copy)]
struct RowContent<'a> {
    name: &'a str,
    live: SubStatus,
    title: SubStatus,
    remove: Option<(u64, &'a str)>,
}

fn channel_row(
    ui: &mut egui::Ui,
    layout: RowLayout,
    content: RowContent<'_>,
    ui_tx: &Sender<UiIntent>,
) {
    let (live_width, title_width, remove_width) = layout.widths;
    table_row(ui, layout.striped, layout.stripe, layout.height, |ui| {
        name_label(
            ui,
            egui::vec2(layout.name_width, layout.height),
            content.name,
            true,
        );
        sub_badge(ui, egui::vec2(live_width, layout.height), content.live);
        sub_badge(ui, egui::vec2(title_width, layout.height), content.title);
        match content.remove {
            Some((id, login)) => {
                let remove = ui.add_sized([remove_width, layout.height], egui::Button::new("×"));
                // Formatted only while hovered: idle rows skip
                // the allocation behind the tooltip.
                let remove = if remove.hovered() {
                    remove.on_hover_text(format!("Remove {login}"))
                } else {
                    remove
                };
                if remove.clicked() {
                    let _ = ui_tx.send(UiIntent::RemoveChannel(id));
                }
            }
            None => {
                ui.add_sized(
                    [remove_width, layout.height],
                    egui::Label::new("…").selectable(false),
                );
            }
        }
    });
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
