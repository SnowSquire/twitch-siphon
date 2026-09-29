//! Slint foreground: builds the retained `AppWindow`, maps the worker
//! `Snapshot` onto its properties, and forwards callbacks as `UiIntent`s.
//!
//! The view owns no state of its own: every pump event clones the latest
//! snapshot out of the shared slot under a short lock and applies it on the
//! UI thread through the weak handle. The worker thread never touches UI.

use std::rc::Rc;
use std::sync::Arc;

use kanal::{Receiver, Sender};
use slint::{ComponentHandle as _, SharedString, VecModel};

use crate::http::ChannelDetail;
use crate::state::{GuiEvent, SharedSnapshot, Snapshot, ThemeMode, UiIntent};
use crate::tray::{Tray, TrayAction};
use crate::ui::{AppWindow, ChannelRow};

/// Everything the Slint bootstrap needs. Channel ends go to the pump
/// thread; the tray and single-instance guard stay alive in `Guards` for
/// the app lifetime.
pub struct ViewParams {
    pub ui_tx: Sender<UiIntent>,
    pub shared: Arc<SharedSnapshot>,
    pub gui_rx: Receiver<GuiEvent>,
    pub tray: Option<Tray>,
    pub single: app_single_instance::PrimaryHandle,
}

/// Owns the tray icon and the primary-instance guard: dropping either
/// removes the icon or releases primary status, so both outlive `run`.
pub struct Guards {
    _tray: Option<Tray>,
    _single: app_single_instance::PrimaryHandle,
}

/// Builds the window, wires callbacks, applies the first snapshot, and
/// spawns the pump thread that carries later worker events onto the UI.
pub fn build(params: ViewParams) -> (AppWindow, Guards) {
    let ui = AppWindow::new().expect("slint window");
    wire(&ui, &params.ui_tx);
    let dark = windows_registry_mode()
        .unwrap_or(ThemeMode::Light)
        .is_dark();
    {
        let snapshot = params.shared.read().clone();
        apply_snapshot(&ui, &snapshot);
        ui.set_dark(dark);
    }
    let has_tray = params.tray.is_some();
    ui.window().on_close_requested(move || {
        if has_tray {
            trim_working_set();
        } else {
            // Without a tray a hidden window would strand invisible, so a
            // real quit instead.
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });
    spawn_pump(&ui, params.shared.clone(), params.gui_rx);
    let guards = Guards {
        _tray: params.tray,
        _single: params.single,
    };
    (ui, guards)
}

/// Subscribes to the documented system theme notification. Runs on the work
/// thread: joining the MTA there cannot disturb the foreground thread's
/// clipboard apartment. The callback reads the reported mode and forwards
/// it through the frame channel; the pump applies it directly, so there is
/// no new channel, no new task, and no polling.
pub fn watch_system_theme(frame: Arc<SharedSnapshot>) {
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

/// Forwards Slint callbacks into the work queue. Empty input submits
/// nothing; a bad row id only logs.
fn wire(ui: &AppWindow, ui_tx: &Sender<UiIntent>) {
    ui.on_add_login({
        let ui_tx = ui_tx.clone();
        move |text| {
            let value = text.trim().to_owned();
            if !value.is_empty() && ui_tx.send(UiIntent::AddLogin(value)).is_err() {
                log::info!(target: "app", "ui intent send failed");
            }
        }
    });
    ui.on_remove_channel({
        let ui_tx = ui_tx.clone();
        move |id| match id.parse::<u64>() {
            Ok(channel) => {
                if ui_tx.send(UiIntent::RemoveChannel(channel)).is_err() {
                    log::info!(target: "app", "ui intent send failed");
                }
            }
            Err(_) => log::info!(target: "app", "bad channel id {id}"),
        }
    });
    ui.on_add_word({
        let ui_tx = ui_tx.clone();
        move |text| {
            let value = text.trim().to_owned();
            if !value.is_empty() && ui_tx.send(UiIntent::AddFilteredWord(value)).is_err() {
                log::info!(target: "app", "ui intent send failed");
            }
        }
    });
    ui.on_remove_word({
        let ui_tx = ui_tx.clone();
        move |index| {
            if index >= 0 {
                let word = index as usize;
                if ui_tx.send(UiIntent::RemoveFilteredWord(word)).is_err() {
                    log::info!(target: "app", "ui intent send failed");
                }
            }
        }
    });
    ui.on_set_notify({
        let ui_tx = ui_tx.clone();
        move |checked| {
            if ui_tx
                .send(UiIntent::SetNotifyTitleChanges(checked))
                .is_err()
            {
                log::info!(target: "app", "ui intent send failed");
            }
        }
    });
    ui.on_set_sound({
        let ui_tx = ui_tx.clone();
        move |checked| {
            if ui_tx.send(UiIntent::SetSound(checked)).is_err() {
                log::info!(target: "app", "ui intent send failed");
            }
        }
    });
    ui.on_apply_update({
        let ui_tx = ui_tx.clone();
        move || {
            if ui_tx.send(UiIntent::ApplyUpdate).is_err() {
                log::info!(target: "app", "ui intent send failed");
            }
        }
    });
    ui.on_clear_error({
        let ui_tx = ui_tx.clone();
        move || {
            if ui_tx.send(UiIntent::ClearError).is_err() {
                log::info!(target: "app", "ui intent send failed");
            }
        }
    });
}

/// Dedicated pump thread: blocks on the worker channel and replays each
/// event onto the UI thread through the weak handle. A dead window ends
/// the loop; the snapshot clone keeps the lock section short.
fn spawn_pump(ui: &AppWindow, shared: Arc<SharedSnapshot>, gui_rx: Receiver<GuiEvent>) {
    let weak = ui.as_weak();
    std::thread::Builder::new()
        .name("slint pump".to_owned())
        .spawn(move || {
            while let Ok(event) = gui_rx.recv() {
                match event {
                    GuiEvent::Snapshot => {
                        let snapshot = shared.read().clone();
                        if weak
                            .upgrade_in_event_loop(move |ui| apply_snapshot(&ui, &snapshot))
                            .is_err()
                        {
                            break;
                        }
                    }
                    GuiEvent::Theme(mode) => {
                        let dark = mode.is_dark();
                        log::info!(target: "app", "applying {} theme", mode.name());
                        if weak
                            .upgrade_in_event_loop(move |ui| ui.set_dark(dark))
                            .is_err()
                        {
                            break;
                        }
                    }
                    GuiEvent::Tray(action) => {
                        // Tray handling reloads the snapshot too.
                        let snapshot = shared.read().clone();
                        if weak
                            .upgrade_in_event_loop(move |ui| {
                                apply_snapshot(&ui, &snapshot);
                                match action {
                                    TrayAction::Show => {
                                        log::info!(target: "tray", "tray Open, showing window");
                                        let _ = ui.window().show();
                                    }
                                    TrayAction::Quit => {
                                        let _ = slint::quit_event_loop();
                                    }
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .expect("slint pump thread");
}

/// Copies one worker snapshot onto the retained properties. Rows follow
/// config order with pending logins appended; the string id round-trips
/// back through `remove-channel` without a Rust-side row mirror.
fn apply_snapshot(ui: &AppWindow, snapshot: &Snapshot) {
    let (text, ok) = connection_text(snapshot.connected, snapshot.conn_error.as_deref());
    ui.set_conn_text(SharedString::from(text));
    ui.set_conn_ok(ok);
    ui.set_error_text(SharedString::from(snapshot.error.as_str()));
    match &snapshot.update {
        Some(offer) => {
            let version = &offer.version;
            ui.set_has_update(true);
            ui.set_update_text(SharedString::from(format!("Update to {version}").as_str()));
        }
        None => {
            ui.set_has_update(false);
            ui.set_update_text(SharedString::new());
        }
    }
    ui.set_notify_titles(snapshot.config.notify_title_changes);
    ui.set_sound(snapshot.config.sound);
    let rows: Vec<ChannelRow> = snapshot
        .config
        .channels
        .iter()
        .map(|channel| {
            let resolved = snapshot.channels.get(&channel.id);
            let name = resolved
                .map(|entry| entry.display_name.as_str())
                .or(channel.display_name.as_deref())
                .unwrap_or(channel.login.as_str());
            let title = resolved
                .and_then(|entry| entry.stream_title.clone())
                .unwrap_or_default();
            let viewers = resolved.map_or("—".to_owned(), live_viewers);
            ChannelRow {
                id: SharedString::from(channel.id.to_string().as_str()),
                name: SharedString::from(name),
                title: SharedString::from(title.as_str()),
                viewers: SharedString::from(viewers.as_str()),
                detail: SharedString::from(detail_line(channel.login.as_str(), resolved).as_str()),
                pending: false,
            }
        })
        .chain(snapshot.pending.iter().map(|login| ChannelRow {
            id: SharedString::new(),
            name: SharedString::from(login.as_str()),
            title: SharedString::from("…"),
            viewers: SharedString::from("…"),
            detail: SharedString::new(),
            pending: true,
        }))
        .collect();
    ui.set_channels(Rc::new(VecModel::from(rows)).into());
    let words: Vec<SharedString> = snapshot
        .config
        .filtered_words
        .iter()
        .map(|word| SharedString::from(word.as_str()))
        .collect();
    ui.set_words(Rc::new(VecModel::from(words)).into());
}

/// Expanded detail under its channel row: the real (login) name alongside
/// the current title, viewers, game, and start time from the last resolve.
fn detail_line(login: &str, channel: Option<&ChannelDetail>) -> String {
    let Some(entry) = channel else {
        return String::new();
    };
    let game = entry
        .game
        .as_ref()
        .map(|game| {
            if game.display_name.is_empty() {
                game.name.as_str()
            } else {
                game.display_name.as_str()
            }
        })
        .unwrap_or("no game");
    let start = format_stream_start(entry.stream_start);
    let counts = viewers_text(entry);
    format!("{login} • {game} • {start} • {counts}")
}

/// Collapsed-row liveness: `offline` when the channel is offline, the
/// viewer count when live. The count also changes while the connection
/// is healthy, so a frozen row reads as a stalled connection.
fn live_viewers(channel: &ChannelDetail) -> String {
    if !channel.live {
        return "offline".to_owned();
    }
    channel
        .viewers
        .map_or("—".to_owned(), |viewers| viewers.to_string())
}

/// Viewers as `viewers/collaboration`, with dashes for unknowns.
fn viewers_text(channel: &ChannelDetail) -> String {
    let viewers = channel
        .viewers
        .map_or("—".to_owned(), |viewers| viewers.to_string());
    let collab = channel
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
    crate::logging::format_iso_ms(millis)
}

fn connection_text(connected: bool, error: Option<&str>) -> (&str, bool) {
    if connected {
        ("Connected", true)
    } else {
        (error.unwrap_or("Connecting…"), false)
    }
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
