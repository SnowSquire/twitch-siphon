use kanal::Sender;
use windows_sys::Win32::UI::WindowsAndMessaging::HICON;
use wgpui::{
    App, AsyncApp, Context, Entity, Render, Subscription, Window, div, prelude::*, px, rgb,
};
use wgpui_kit::base::Disableable as _;
use wgpui_kit::base::{Checkbox, CheckboxIndicator, CheckboxState};
use wgpui_kit::component::Root;
use wgpui_kit::component::button::Button;
use wgpui_kit::component::input::{Input, InputEvent, InputState};
use wgpui_kit::component::scroll::ScrollableElement as _;
use wgpui_kit::component::theme::{Theme, ThemeMode};
use wgpui_kit::component::tooltip::Tooltip;

use crate::state::{AppState, ChannelStatus, GuiEvent, SharedFrame, SubStatus, UiIntent, UpdateStatus};
use crate::tray::{Tray, TrayAction};

const GREEN: u32 = 0x2e_a043;
const RED: u32 = 0xda_3633;
const GRAY: u32 = 0x6e_7681;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Channels,
    FilteredWords,
}

/// Everything the window builder needs beyond the wgpui contexts. Moved into
/// the builder closure; the channel ends go to the pump task, the rest lives
/// on the view entity.
pub struct WindowParams {
    pub ui_tx: Sender<UiIntent>,
    pub shared: SharedFrame,
    pub gui_rx: kanal::AsyncReceiver<GuiEvent>,
    pub tray: Option<Tray>,
    pub single: app_single_instance::PrimaryHandle,
}

pub struct SiphonView {
    ui_tx: Sender<UiIntent>,
    shared: SharedFrame,
    seen_version: u64,
    frame: AppState,
    login_input: Entity<InputState>,
    word_input: Entity<InputState>,
    tab: Tab,
    /// Expanded channel row, showing the resolved detail between the rows.
    /// Purely presentational: toggled by clicking the channel name.
    expanded: Option<u64>,
    /// `None` when the tray icon failed to build: closing the window quits
    /// instead of hiding, so it can never strand invisible without a tray.
    /// Also owns the icon; dropping removes it from the tray.
    tray: Option<Tray>,
    /// Held alive for the app lifetime; the guard keeps this instance
    /// primary, so a second launch wakes it instead of starting over.
    _single: app_single_instance::PrimaryHandle,
    hwnd: Option<isize>,
    /// The title/taskbar icon installed by [`build`]; the window only
    /// stores the handle, so the view owns it until drop.
    window_icon: Option<HICON>,
    _subs: Vec<Subscription>,
}

impl SiphonView {
    /// Builds the view entity, wires input events and the work→GUI pump, and
    /// wraps it in the Kit root the window renders.
    pub fn build(window: &mut Window, cx: &mut App, params: WindowParams) -> Entity<Root> {
        let login_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add new streamer"));
        let word_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add filtered word"));
        let frame = params.shared.read().clone();
        let seen_version = params.shared.version();
        let hwnd = win_hwnd(window);
        let window_icon = hwnd.and_then(set_window_icon);
        let view = cx.new(|_cx| SiphonView {
            ui_tx: params.ui_tx,
            shared: params.shared,
            seen_version,
            frame,
            login_input: login_input.clone(),
            word_input: word_input.clone(),
            tab: Tab::Channels,
            expanded: None,
            tray: params.tray,
            _single: params.single,
            hwnd,
            window_icon,
            _subs: Vec::new(),
        });

        // Enter in either field submits it, the same as the Add button.
        for (input, intent) in [
            (login_input, UiIntent::AddLogin as fn(String) -> UiIntent),
            (
                word_input,
                UiIntent::AddFilteredWord as fn(String) -> UiIntent,
            ),
        ] {
            view.update(cx, |view, cx| {
                let ui_tx = view.ui_tx.clone();
                let sub =
                    cx.subscribe_in(&input, window, move |_view, state, event, window, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            submit_input(state, window, cx, &ui_tx, intent);
                        }
                    });
                view._subs.push(sub);
            });
        }

        spawn_pump(cx, &view, params.gui_rx);
        sync_theme(window, cx);

        let close_hwnd = view.read(cx).hwnd;
        let has_tray = view.read(cx).tray.is_some();
        window.on_window_should_close(cx, move |_window, cx| {
            if !has_tray {
                // Without a tray the window would strand invisible, so
                // a real quit instead. Explicit quit mode means closing
                // the last window alone would linger without one.
                cx.quit();
                return true;
            }
            if let Some(hwnd) = close_hwnd {
                hide_window(hwnd);
                log::info!(target: "tray", "close hid to tray");
            }
            false
        });

        cx.new(|cx| Root::new(view, window, cx))
    }
}

/// Applies the OS light/dark setting once. Later changes arrive as theme
/// events from the system watcher. Kit components render from the global
/// theme, so a change repaints the whole tree without touching view state.
fn sync_theme(window: &mut Window, cx: &mut App) {
    apply_system_theme(window, cx);
}

fn apply_system_theme(window: &mut Window, cx: &mut App) {
    let mode = windows_registry_mode().unwrap_or_else(|| ThemeMode::from(window.appearance()));
    log::info!(target: "app", "applying {} theme", mode.name());
    Theme::change(mode, Some(window), cx);
    if let Some(hwnd) = win_hwnd(window) {
        {
            use windows_sys::Win32::Foundation::HWND;
            use windows_sys::Win32::Graphics::Dwm::{
                DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
            };
            use windows_sys::core::BOOL;

            let dark: BOOL = i32::from(mode.is_dark());

            unsafe {
                DwmSetWindowAttribute(
                    hwnd as HWND,
                    DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                    core::ptr::from_ref(&dark).cast(),
                    std::mem::size_of::<BOOL>() as u32,
                );
            }
        };
    }
}

/// The "default app mode" setting: 0 is dark, anything else is light.
fn windows_registry_mode() -> Option<ThemeMode> {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
        .and_then(|key| key.get_value::<u32, _>("AppsUseLightTheme"))
        .map(|light| {
            if light == 0 {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            }
        })
        .ok()
}

/// Subscribes to the documented system theme notification. Runs on the work
/// thread: joining the MTA there cannot disturb the foreground thread's
/// clipboard apartment. The callback reads the reported mode and forwards
/// it through the frame channel; the pump applies it directly, so
/// there is no new channel, no new task, and no polling.
pub fn watch_system_theme(frame: SharedFrame) {
    use windows::Foundation::TypedEventHandler;
    use windows::UI::ViewManagement::UISettings;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    // SAFETY: the work thread is COM-free; the MTA join affects only it.
    if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
        log::info!(target: "app", "theme watch COM init failed");
        return;
    }
    let settings = match UISettings::new() {
        Ok(settings) => settings,
        Err(error) => {
            log::info!(target: "app", "theme watch settings failed: {error}");
            return;
        }
    };
    let registration = settings.ColorValuesChanged(&TypedEventHandler::new(move |_, _| {
        match windows_registry_mode() {
            Some(mode) => {
                log::info!(target: "app", "system theme changed to {}", mode.name());
                frame.notify_theme(mode);
            }
            None => log::info!(target: "app", "system theme changed, app mode unreadable"),
        }
        Ok(())
    }));
    match registration {
        Ok(token) => {
            log::info!(target: "app", "theme watch registered");
            // Held for the app lifetime; dropping the settings object would
            // revoke the subscription.
            Box::leak(Box::new((settings, token)));
        }
        Err(error) => log::info!(target: "app", "theme watch subscribe failed: {error}"),
    }
}

/// Foreground task awaiting work→GUI events. Nothing polls: the channel
/// parks the task until the work thread sends.
fn spawn_pump(cx: &mut App, view: &Entity<SiphonView>, gui_rx: kanal::AsyncReceiver<GuiEvent>) {
    let weak = view.downgrade();
    cx.spawn(async move |cx| {
        loop {
            match gui_rx.recv().await {
                Ok(GuiEvent::Frame) => {
                    if reload_frame(&weak, cx).is_err() {
                        break;
                    }
                }
                Ok(GuiEvent::Theme(mode)) => {
                    if apply_theme_event(&weak, cx, mode).is_err() {
                        break;
                    }
                }
                Ok(GuiEvent::Tray(action)) => {
                    if reload_frame(&weak, cx).is_err() || handle_tray(cx, &weak, action).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
    .detach();
}

/// Copies a newer snapshot into the view.
/// Reports whether the view is still alive.
fn reload_frame(weak: &wgpui::WeakEntity<SiphonView>, cx: &mut AsyncApp) -> wgpui::Result<()> {
    weak.update(cx, |view, cx| {
        let fresh = view.shared.version();
        if fresh == view.seen_version {
            return;
        }
        view.frame = view.shared.read().clone();
        view.seen_version = fresh;
        cx.notify();
    })
}

/// Applies a system theme event. The watcher reported the mode, so it
/// applies directly with no re-read; the notify repaints with the new
/// colors. Reports whether the view is still alive.
fn apply_theme_event(
    weak: &wgpui::WeakEntity<SiphonView>,
    cx: &mut AsyncApp,
    mode: ThemeMode,
) -> wgpui::Result<()> {
    weak.update(cx, |_view, cx| {
        log::info!(target: "app", "applying {} theme", mode.name());
        Theme::change(mode, None, cx);
        cx.notify();
    })
}

/// Applies one tray or single-instance action. Shows the window or quits the
/// app; both ride the pump so no thread ever touches UI state directly.
fn handle_tray(
    cx: &mut AsyncApp,
    weak: &wgpui::WeakEntity<SiphonView>,
    action: TrayAction,
) -> wgpui::Result<()> {
    match action {
        TrayAction::Show => {
            log::info!(target: "tray", "tray Open, showing window");
            let hwnd = weak.update(cx, |view, _cx| view.hwnd)?;
            if let Some(hwnd) = hwnd {
                show_window(hwnd);
            }
        }
        TrayAction::Quit => {
            cx.update(|cx| cx.quit()).ok();
        }
    }
    Ok(())
}

/// Reads an input field, clears it, and sends the value as an intent. Empty
/// input submits nothing.
fn submit_input(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
    ui_tx: &Sender<UiIntent>,
    intent: fn(String) -> UiIntent,
) {
    let value: String = input.read(cx).value().trim().to_owned();
    if value.is_empty() {
        return;
    }
    input.update(cx, |state, cx| state.set_value("", window, cx));
    if ui_tx.send(intent(value)).is_err() {
        log::info!(target: "app", "ui intent send failed");
    }
}

impl Render for SiphonView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .size_full()
            .child(header(self, cx))
            .child(tabs(self, cx));

        let mut middle = div()
            .flex()
            .flex_col()
            .flex_1()
            .gap_2()
            .overflow_y_scrollbar();

        match self.tab {
            Tab::Channels => middle = middle.child(channel_list(self, cx)),
            Tab::FilteredWords => middle = middle.child(word_list(self, cx)),
        }

        // The middle content absorbs free space, so the input row and
        // toggles below stay pinned to the bottom of the window.
        root.child(middle)
            .child(input_row(self, cx))
            .child(toggles(self, cx))
            .child(error_row(self))
    }
}

fn header(view: &SiphonView, _cx: &mut Context<SiphonView>) -> impl IntoElement {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(div().text_xl().child("Siphon"))
        .child(connection(view));

    match &view.frame.update {
        UpdateStatus::Available(offer) => {
            let ui_tx = view.ui_tx.clone();
            row = row.child(
                Button::new("update")
                    .label(format!("Update to {}", offer.version))
                    .on_click(move |_, _, _| {
                        let _ = ui_tx.send(UiIntent::ApplyUpdate);
                    }),
            );
        }
        UpdateStatus::Downloading(_) => {
            row = row.child(Button::new("update").label("Updating…").disabled(true));
        }
        UpdateStatus::Idle | UpdateStatus::Checking | UpdateStatus::Current => {}
    }
    row
}

fn connection(view: &SiphonView) -> impl IntoElement {
    let (text, color) = connection_text(view.frame.connected, view.frame.conn_error.as_deref());
    div().text_color(rgb(color)).child(text.to_owned())
}

fn tabs(view: &SiphonView, cx: &mut Context<SiphonView>) -> impl IntoElement {
    let channels = Button::new("tab-channels")
        .label(if view.tab == Tab::Channels {
            "» Channels"
        } else {
            "Channels"
        })
        .on_click(cx.listener(|this: &mut SiphonView, _event, _window, cx| {
            this.tab = Tab::Channels;
            cx.notify();
        }));
    let words = Button::new("tab-words")
        .label(if view.tab == Tab::FilteredWords {
            "» Filtered words"
        } else {
            "Filtered words"
        })
        .on_click(cx.listener(|this: &mut SiphonView, _event, _window, cx| {
            this.tab = Tab::FilteredWords;
            cx.notify();
        }));
    div().flex().flex_row().gap_2().child(channels).child(words)
}

fn channel_list(view: &SiphonView, cx: &mut Context<SiphonView>) -> impl IntoElement {
    if view.frame.config.channels.is_empty() && view.frame.pending.is_empty() {
        return div().child("No channels configured. Add a streamer below.");
    }
    let mut list = div().flex().flex_col().gap_1();
    list = list.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(div().flex_1().child("Channel"))
            .child(div().w(px(110.)).child("Live Status"))
            .child(div().w(px(110.)).child("Title Status"))
            .child(div().w(px(36.)).child("")),
    );
    for channel in &view.frame.config.channels {
        let resolved = view
            .frame
            .channels
            .iter()
            .find(|entry| entry.channel_id == channel.id);
        let name = resolved
            .map(|entry| entry.display_name.as_str())
            .or(channel.display_name.as_deref())
            .unwrap_or(channel.login.as_str());
        let (live, title) = resolved.map_or((SubStatus::Pending, SubStatus::Pending), |entry| {
            (entry.live_status, entry.title_status)
        });
        let id = channel.id;
        let login = channel.login.clone();
        let ui_tx = view.ui_tx.clone();
        let expanded = view.expanded == Some(id);
        let channel_tip = format!("id {id}");
        let name_cell = div().flex_1().child(
            div()
                .id(format!("channel-name-{id}"))
                .cursor_pointer()
                .on_click(cx.listener(move |this: &mut SiphonView, _event, _window, cx| {
                    this.expanded = if this.expanded == Some(id) { None } else { Some(id) };
                    cx.notify();
                }))
                .tooltip(move |window, cx| Tooltip::new(channel_tip.clone()).build(window, cx))
                .child(name.to_owned()),
        );
        // The detail renders below the row, never inside it, so the name,
        // badges, and close button keep their positions when it opens.
        let mut wrapper = div().flex().flex_col().gap_1().child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .child(name_cell)
                .child(div().w(px(110.)).child(sub_badge(live)))
                .child(div().w(px(110.)).child(sub_badge(title)))
                .child(Button::new(format!("remove-{id}")).label("×").on_click(
                    move |_, _, _| {
                        log::info!(target: "app", "remove requested for {login}");
                        let _ = ui_tx.send(UiIntent::RemoveChannel(id));
                    },
                )),
        );
        if expanded && let Some(entry) = resolved {
            wrapper = wrapper.child(channel_detail(entry, cx));
        }
        list = list.child(wrapper);
    }
    for login in &view.frame.pending {
        list = list.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .child(div().flex_1().child(login.clone()))
                .child(div().w(px(110.)).child(sub_badge(SubStatus::Pending)))
                .child(div().w(px(110.)).child(sub_badge(SubStatus::Pending)))
                .child(div().w(px(36.)).child("…")),
        );
    }
    list
}

/// Expanded detail under its channel row: the real (login) name alongside
/// the current title, viewers, game, and start time from the last resolve.
/// The muted background groups it with the row above. Hovering the login
/// shows the channel id, hovering the title shows the stream id, hovering
/// the game shows the game id.
fn channel_detail(entry: &ChannelStatus, cx: &App) -> impl IntoElement {
    let channel_tip = format!("id {}", entry.channel_id);
    let stream_tip = if entry.stream_id == 0 {
        "no stream".to_owned()
    } else {
        format!("stream {}", entry.stream_id)
    };
    let game_tip = entry
        .game_id
        .map_or("no game".to_owned(), |id| format!("game {id}"));
    div()
        .flex()
        .flex_col()
        .gap_1()
        .text_sm()
        .rounded(px(4.))
        .p_2()
        .bg(Theme::global(cx).muted)
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .id(format!("channel-login-{}", entry.channel_id))
                        .tooltip(move |window, cx| {
                            Tooltip::new(channel_tip.clone()).build(window, cx)
                        })
                        .child(entry.login.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .id(format!("stream-title-{}", entry.channel_id))
                        .tooltip(move |window, cx| {
                            Tooltip::new(stream_tip.clone()).build(window, cx)
                        })
                        .child(
                            entry
                                .stream_title
                                .clone()
                                .unwrap_or_else(|| "no title".to_owned()),
                        ),
                )
                .child(div().child(viewers_text(entry))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(div().flex_1().child(format_stream_start(entry.stream_start)))
                .child(
                    div()
                        .flex_1()
                        .id(format!("game-{}", entry.channel_id))
                        .tooltip(move |window, cx| {
                            Tooltip::new(game_tip.clone()).build(window, cx)
                        })
                        .child(
                            entry.game.clone().unwrap_or_else(|| "no game".to_owned()),
                        ),
                ),
        )
}

/// Viewers as `viewers/collaboration`, with dashes for unknowns.
fn viewers_text(entry: &ChannelStatus) -> String {
    let viewers = entry
        .viewers
        .map_or("—".to_owned(), |viewers| viewers.to_string());
    let collab = entry
        .collaboration_viewers
        .map_or("—".to_owned(), |collab| collab.to_string());
    format!("{viewers}/{collab}")
}

/// Millis-since-epoch to `YYYY-MM-DDTHH:MM:SS.mmmZ` without a date crate.
/// `None` (offline, never started) renders as an em dash.
fn format_stream_start(start: Option<i64>) -> String {
    let Some(millis) = start.filter(|millis| *millis >= 0) else {
        return "—".to_owned();
    };
    let secs = millis / 1000;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3600,
        time % 3600 / 60,
        time % 60,
        millis % 1000,
    )
}

/// Days since 1970-01-01 to calendar date.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (
        if month <= 2 { year + 1 } else { year },
        month,
        day,
    )
}

fn word_list(view: &SiphonView, _cx: &mut Context<SiphonView>) -> impl IntoElement {    let mut row = div().flex().flex_row().flex_wrap().gap_1();
    for (index, word) in view.frame.config.filtered_words.iter().enumerate() {
        let ui_tx = view.ui_tx.clone();
        let label = format!("{word} ×");
        row = row.child(Button::new(format!("word-{index}")).label(label).on_click(
            move |_, _, _| {
                let _ = ui_tx.send(UiIntent::RemoveFilteredWord(index));
            },
        ));
    }
    if view.frame.config.filtered_words.is_empty() {
        row = row.child("No filtered words. Add one below.");
    }
    row
}

fn input_row(view: &SiphonView, _cx: &mut Context<SiphonView>) -> impl IntoElement {
    let (input, intent): (&Entity<InputState>, fn(String) -> UiIntent) = match view.tab {
        Tab::Channels => (&view.login_input, UiIntent::AddLogin),
        Tab::FilteredWords => (&view.word_input, UiIntent::AddFilteredWord),
    };
    let input = input.clone();
    let ui_tx = view.ui_tx.clone();
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(div().flex_1().child(Input::new(&input)))
        .child(
            Button::new("add")
                .label("Add")
                .on_click(move |_, window, cx| {
                    submit_input(&input, window, cx, &ui_tx, intent);
                }),
        )
}

fn toggles(view: &SiphonView, cx: &mut Context<SiphonView>) -> impl IntoElement {
    let theme = Theme::global(cx);
    let dark = theme.is_dark();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .text_color(theme.foreground)
        .child(toggle_row(
            "notify-titles",
            "Notify on title changes while offline",
            view.frame.config.notify_title_changes,
            dark,
            view.ui_tx.clone(),
            UiIntent::SetNotifyTitleChanges,
        ))
        .child(toggle_row(
            "sound",
            "Notification sound",
            view.frame.config.sound,
            dark,
            view.ui_tx.clone(),
            UiIntent::SetSound,
        ))
}

/// Solid-fill box colors, picked per theme mode so the state reads at a
/// glance: green when checked, monochrome when not. The tick contrasts
/// with the checked fill it sits on.
const CHECKED_FILL_LIGHT: u32 = 0x16_6534;
const CHECKED_FILL_DARK: u32 = 0x4a_de80;
const UNCHECKED_FILL_LIGHT: u32 = 0x00_0000;
const UNCHECKED_FILL_DARK: u32 = 0xff_ffff;
const TICK_ON_DARK_GREEN: u32 = 0xff_ffff;
const TICK_ON_LIGHT_GREEN: u32 = 0x00_0000;

/// One labeled toggle on the unstyled base checkbox, which keeps toggle,
/// focus, keyboard, and accessibility behavior while the app owns every
/// pixel of the box.
fn toggle_row(
    id: &'static str,
    label: &'static str,
    checked: bool,
    dark: bool,
    ui_tx: Sender<UiIntent>,
    intent: fn(bool) -> UiIntent,
) -> impl IntoElement {
    let fill = if checked {
        if dark {
            CHECKED_FILL_DARK
        } else {
            CHECKED_FILL_LIGHT
        }
    } else if dark {
        UNCHECKED_FILL_DARK
    } else {
        UNCHECKED_FILL_LIGHT
    };
    let tick = if dark {
        TICK_ON_LIGHT_GREEN
    } else {
        TICK_ON_DARK_GREEN
    };
    Checkbox::new(id)
        .checked(checked)
        .accessibility_label(label)
        .on_change(move |state, _event, _window, _cx| {
            let _ = ui_tx.send(intent(matches!(state, CheckboxState::Checked)));
        })
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .child(
            CheckboxIndicator::new()
                .checked(checked)
                .size_4()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .border_1()
                .rounded(px(4.))
                .bg(rgb(fill))
                .border_color(rgb(fill))
                .text_color(rgb(tick))
                .text_sm()
                .child(if checked { div().child("✓") } else { div() }),
        )
        .child(div().child(label.to_owned()))
}

fn error_row(view: &SiphonView) -> impl IntoElement {
    if view.frame.error.is_empty() {
        return div();
    }
    let ui_tx = view.ui_tx.clone();
    div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .text_color(rgb(RED))
                .child(view.frame.error.clone()),
        )
        .child(Button::new("dismiss").label("×").on_click(move |_, _, _| {
            let _ = ui_tx.send(UiIntent::ClearError);
        }))
}

fn connection_text(connected: bool, error: Option<&str>) -> (&str, u32) {
    if connected {
        ("Connected", GREEN)
    } else {
        (error.unwrap_or("Connecting…"), RED)
    }
}

fn sub_badge(state: SubStatus) -> impl IntoElement {
    let (text, color) = match state {
        SubStatus::Pending => ("Pending", GRAY),
        SubStatus::Connected => ("Connected", GREEN),
        SubStatus::Failed => ("Failed", RED),
    };
    div().text_color(rgb(color)).child(text)
}

/// Raw Win32 handle for our own window. wgpui exposes no hide API, so
/// visibility goes through Win32, the same approach as the eframe app.
fn win_hwnd(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    HasWindowHandle::window_handle(window)
        .ok()
        .and_then(|handle| match handle.as_raw() {
            RawWindowHandle::Win32(win) => Some(win.hwnd.get()),
            _ => None,
        })
}

/// Assigns the bundled icon to our own window for the taskbar and title
/// bar; wgpui exposes no icon API, so this goes through `WM_SETICON`
/// directly. Returns the icon for the view to own: the window only stores
/// the handle, so freeing it would blank the icon.
fn set_window_icon(hwnd: isize) -> Option<HICON> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging as wam;

    let icon = match crate::tray::load_icon() {
        Ok(icon) => icon,
        Err(error) => {
            log::error!(target: "app", "window icon load failed: {error}");
            return None;
        }
    };
    // SAFETY: `hwnd` is our own live window; `WM_SETICON` only stores the
    // handle for the shell to paint.
    unsafe {
        wam::SendMessageW(
            hwnd as HWND,
            wam::WM_SETICON,
            wam::ICON_SMALL as WPARAM,
            icon as LPARAM,
        );
        wam::SendMessageW(hwnd as HWND, wam::WM_SETICON, wam::ICON_BIG as WPARAM, icon as LPARAM);
    }
    Some(icon)
}

impl Drop for SiphonView {
    fn drop(&mut self) {
        if let Some(icon) = self.window_icon {
            use windows_sys::Win32::UI::WindowsAndMessaging as wam;

            // SAFETY: icon installed by `set_window_icon` and owned by
            // this view.
            unsafe { wam::DestroyIcon(icon) };
        }
    }
}

/// The close button hides to the tray; without a tray it would strand
/// invisible, so this helper is only used when one exists.
fn hide_window(hwnd: isize) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging as wam;

    // SAFETY: `hwnd` is our own live window; `ShowWindow` only toggles
    // visibility.
    unsafe {
        wam::ShowWindow(hwnd as HWND, wam::SW_HIDE);
    }
    trim_working_set();
}

/// A hidden tray app needs no resident pages: page everything out and let
/// faults bring back only what the worker threads touch.
fn trim_working_set() {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

    // SAFETY: the counters are a plain struct we own; the process handle is
    // ours.
    unsafe {
        let process = GetCurrentProcess();
        SetProcessWorkingSetSize(process, usize::MAX, usize::MAX);
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        if GetProcessMemoryInfo(
            process,
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ) != 0
        {
            log::info!(
                target: "app",
                "trimmed working set to {} MB",
                counters.WorkingSetSize / 1024 / 1024,
            );
        }
    }
}

/// Unhide + focus our own window.
fn show_window(hwnd: isize) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging as wam;

    log::info!(target: "tray", "show requested (hwnd={hwnd:?})");
    // SAFETY: `hwnd` is our own live window.
    unsafe {
        wam::ShowWindow(hwnd as HWND, wam::SW_SHOWDEFAULT);
        wam::SetForegroundWindow(hwnd as HWND);
    }
}

#[cfg(test)]
mod tests {
    use super::format_stream_start;

    #[test]
    fn stream_start_formats_utc() {
        assert_eq!(format_stream_start(None), "—");
        assert_eq!(format_stream_start(Some(-1)), "—");
        assert_eq!(
            format_stream_start(Some(0)),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_stream_start(Some(1_000_000_000_000)),
            "2001-09-09T01:46:40.000Z"
        );
        assert_eq!(
            format_stream_start(Some(1_709_208_000_000)),
            "2024-02-29T12:00:00.000Z"
        );
        assert_eq!(
            format_stream_start(Some(1_788_237_802_868)),
            "2026-09-01T04:43:22.868Z"
        );
    }
}
