//! wgpui presenter for Siphon.
//!
//! One root view owns a snapshot of the latest [`FrameState`] plus the two
//! input fields. A foreground pump folds work→GUI events into it; every
//! mutation goes work-ward as a [`UiIntent`]. Closing the
//! window hides to the tray instead of quitting.

use kanal::Sender;
#[cfg(not(windows))]
use wgpui::AnyWindowHandle;
#[cfg(not(windows))]
use wgpui::BorrowAppContext as _;
use wgpui::{
    App, AsyncApp, Context, Entity, Render, Subscription, Window, div, prelude::*, px, rgb,
};
use wgpui_kit::base::Disableable as _;
use wgpui_kit::component::Root;
use wgpui_kit::component::button::Button;
use wgpui_kit::component::checkbox::Checkbox;
use wgpui_kit::component::input::{Input, InputEvent, InputState};
use wgpui_kit::component::scroll::ScrollableElement as _;
use wgpui_kit::component::theme::{Theme, ThemeMode};

use crate::hermes::{StatusSnapshot, SubStatus};
use crate::state::{FrameState, GuiEvent, GuiSender, SharedFrame, UiIntent, UpdateStatus};
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
    frame: FrameState,
    login_input: Entity<InputState>,
    word_input: Entity<InputState>,
    tab: Tab,
    /// `None` when the tray icon failed to build: closing the window quits
    /// instead of hiding, so it can never strand invisible without a tray.
    /// Also owns the icon; dropping removes it from the tray.
    tray: Option<Tray>,
    /// Held alive for the app lifetime; the guard keeps this instance
    /// primary, so a second launch wakes it instead of starting over.
    _single: app_single_instance::PrimaryHandle,
    /// Window handle for platforms without a Win32 `HWND`: tray Show
    /// restores a minimized window through it. Windows hides through
    /// `hwnd` instead, so this only exists elsewhere.
    #[cfg(not(windows))]
    window: AnyWindowHandle,
    #[cfg(windows)]
    hwnd: Option<isize>,
    _subs: Vec<Subscription>,
}

impl SiphonView {
    /// Builds the view entity, wires input events and the work→GUI pump, and
    /// wraps it in the Kit root the window renders.
    pub fn build(window: &mut Window, cx: &mut App, params: WindowParams) -> Entity<Root> {
        let login_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add new streamer"));
        let word_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add filtered word"));
        let (frame, seen_version) = params.shared.load();
        #[cfg(windows)]
        let hwnd = win_hwnd(window);
        let view = cx.new(|_cx| SiphonView {
            ui_tx: params.ui_tx,
            shared: params.shared,
            seen_version,
            frame,
            login_input: login_input.clone(),
            word_input: word_input.clone(),
            tab: Tab::Channels,
            tray: params.tray,
            _single: params.single,
            #[cfg(not(windows))]
            window: window.window_handle(),
            #[cfg(windows)]
            hwnd,
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
        sync_theme(window, cx, &view);

        #[cfg(windows)]
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
            #[cfg(windows)]
            if let Some(hwnd) = close_hwnd {
                hide_window(hwnd);
                log::info!(target: "tray", "close hid to tray");
            }
            #[cfg(not(windows))]
            _window.minimize_window();
            false
        });

        cx.new(|cx| Root::new(view, window, cx))
    }
}

/// Applies the OS light/dark setting once. Later changes arrive as theme
/// events from the system watcher on Windows, through window appearance
/// elsewhere. Kit components render from the global theme, so a change
/// repaints the whole tree without touching view state.
fn sync_theme(window: &mut Window, cx: &mut App, _view: &Entity<SiphonView>) {
    apply_system_theme(window, cx);
    #[cfg(not(windows))]
    {
        let sub = window.observe_window_appearance(|window, cx| {
            apply_system_theme(window, cx);
        });
        _view.update(cx, |view, _cx| view._subs.push(sub));
    }
}

fn apply_system_theme(window: &mut Window, cx: &mut App) {
    #[cfg(windows)]
    let mode = windows_registry_mode().unwrap_or_else(|| ThemeMode::from(window.appearance()));
    #[cfg(not(windows))]
    let mode = ThemeMode::from(window.appearance());
    log::info!(target: "app", "applying {} theme", mode.name());
    Theme::change(mode, Some(window), cx);
    #[cfg(windows)]
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
#[cfg(windows)]
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
/// it through the existing gui sender; the pump applies it directly, so
/// there is no new channel, no new task, and no polling.
#[cfg(windows)]
pub fn watch_system_theme(gui: GuiSender) {
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
                gui.notify_theme(mode);
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
        if let Some((frame, version)) = view.shared.load_newer(view.seen_version) {
            view.frame = frame;
            view.seen_version = version;
            cx.notify();
        }
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
            #[cfg(windows)]
            {
                let hwnd = weak.update(cx, |view, _cx| view.hwnd)?;
                if let Some(hwnd) = hwnd {
                    show_window(hwnd);
                }
            }
            #[cfg(not(windows))]
            {
                let window = weak.update(cx, |view, _cx| view.window)?;
                cx.update_window(window, |_, window, _| {
                    window.activate_window();
                })
                .ok();
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
        let pending: Vec<String> = self
            .frame
            .status
            .as_ref()
            .and_then(|snapshot| snapshot.pending_adds.read().ok())
            .map_or_else(Vec::new, |guard| guard.clone());

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
            Tab::Channels => middle = middle.child(channel_list(self, cx, &pending)),
            Tab::FilteredWords => middle = middle.child(word_list(self, cx)),
        }

        // The middle content absorbs free space, so the input row and
        // toggles below stay pinned to the bottom of the window.
        root.child(middle)
            .child(input_row(self, cx))
            .child(toggles(self))
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
    let (text, color) = connection_text(view.frame.status.as_ref());
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

fn channel_list(
    view: &SiphonView,
    _cx: &mut Context<SiphonView>,
    pending: &[String],
) -> impl IntoElement {
    if view.frame.config.channels.is_empty() && pending.is_empty() {
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
        let resolved = view.frame.status.as_ref().and_then(|snapshot| {
            snapshot
                .channels
                .iter()
                .find(|entry| entry.channel_id == channel.id)
        });
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
        list =
            list.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(name.to_owned()))
                    .child(div().w(px(110.)).child(sub_badge(live)))
                    .child(div().w(px(110.)).child(sub_badge(title)))
                    .child(Button::new(format!("remove-{id}")).label("×").on_click(
                        move |_, _, _| {
                            log::info!(target: "app", "remove requested for {login}");
                            let _ = ui_tx.send(UiIntent::RemoveChannel(id));
                        },
                    )),
            );
    }
    for login in pending {
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

fn word_list(view: &SiphonView, _cx: &mut Context<SiphonView>) -> impl IntoElement {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_1()
        .child("Titles containing these words stay silent for title-change notifications.");
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

fn toggles(view: &SiphonView) -> impl IntoElement {
    let ui_tx = view.ui_tx.clone();
    let notify = Checkbox::new("notify-titles")
        .label("Notify on title changes while offline")
        .checked(view.frame.config.notify_title_changes)
        .on_click(move |checked: &bool, _, _| {
            let _ = ui_tx.send(UiIntent::SetNotifyTitleChanges(*checked));
        });
    let ui_tx = view.ui_tx.clone();
    let sound = Checkbox::new("sound")
        .label("Notification sound")
        .checked(view.frame.config.sound)
        .on_click(move |checked: &bool, _, _| {
            let _ = ui_tx.send(UiIntent::SetSound(*checked));
        });
    div().flex().flex_col().gap_1().child(notify).child(sound)
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

fn connection_text(status: Option<&StatusSnapshot>) -> (&str, u32) {
    match status {
        Some(snapshot) if snapshot.connected => ("Connected", GREEN),
        Some(snapshot) => (snapshot.error.as_deref().unwrap_or("Connecting…"), RED),
        None => ("Connecting…", GRAY),
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
#[cfg(windows)]
fn win_hwnd(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    HasWindowHandle::window_handle(window)
        .ok()
        .and_then(|handle| match handle.as_raw() {
            RawWindowHandle::Win32(win) => Some(win.hwnd.get()),
            _ => None,
        })
}

/// The close button hides to the tray; without a tray it would strand
/// invisible, so this helper is only used when one exists.
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
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
