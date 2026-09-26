// No console window in release on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod balesh;
mod config;
mod event_loop;
mod hermes;
mod http;
mod logging;
mod matcher;
mod notifier;
mod state;
mod tray;
mod update;
use wgpui::{App, Application, Bounds, QuitMode, WindowBounds, WindowOptions, px, size};

use crate::app::{SiphonView, WindowParams};
use crate::config::Config;
use crate::state::{AppState, SharedFrame, UiIntent, UpdateStatus, Worker, WorkerParams};
use crate::tray::TrayAction;

/// Single-instance key and config directory name. Debug builds use a
/// separate id so a debug binary runs alongside the installed release
/// without waking it or sharing its config.
#[cfg(debug_assertions)]
const APP_ID: &str = "com.iken.siphon.debug";
#[cfg(not(debug_assertions))]
const APP_ID: &str = "com.iken.siphon";

fn main() {
    match crate::logging::init(APP_ID) {
        Some(path) => log::info!(target: "app", "logging to {}", path.display()),
        None => log::info!(target: "app", "logging to stdout"),
    }

    // A second launch wakes the first instance and exits immediately.
    if app_single_instance::notify_if_running(APP_ID) {
        return;
    }

    notifier::register_aumid();

    let config_path = dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_ID)
        .join("config.json");
    let config = Config::load(&config_path);
    log::info!(
        target: "config",
        "loaded config from {}: {} channel(s) {:?}, title_changes={}, sound={}",
        config_path.display(),
        config.channels.len(),
        config
            .channels
            .iter()
            .map(|channel| channel.login.as_str())
            .collect::<Vec<_>>(),
        config.notify_title_changes,
        config.sound,
    );

    let (ui_tx, ui_rx) = kanal::unbounded::<UiIntent>();
    let (tray_tx, tray_rx) = kanal::unbounded::<TrayAction>();
    // Latest-only snapshot slot shared by the work thread (writer) and the
    // GUI thread (reader); every store wakes the foreground pump.
    let (frame, gui_rx) = SharedFrame::pair(AppState {
        config: config.clone(),
        connected: false,
        conn_error: None,
        channels: Vec::new(),
        pending: Vec::new(),
        error: String::new(),
        update: UpdateStatus::Idle,
    });

    //worker thread, runs hermes and event loop
    let work_frame = frame.clone();
    let wake_frame = frame.clone();
    let theme_frame = frame.clone();
    let _work_thread = std::thread::Builder::new()
        .name("worker thread".to_owned())
        .spawn(move || {
            // System theme notifications land here: no new thread, and COM
            // stays off the foreground thread's clipboard apartment.
            crate::app::watch_system_theme(theme_frame);
            {
                compio::runtime::Runtime::new()
                    .expect("compio runtime")
                    .block_on(async move {
                        let mut worker = Worker::new(WorkerParams {
                            config_path,
                            config,
                            ui_rx,
                            tray_rx,
                            shared: work_frame,
                        });
                        worker.publish();
                        worker.push_update_check();
                        worker.run().await;
                    });
            };
        })
        .expect("hermes thread");

    // Second launches wake the window through the frame channel the
    // foreground pump awaits, so showing works even while hidden.
    let single = app_single_instance::start_primary(APP_ID, move || {
        log::info!(target: "single", "wake signal received");
        wake_frame.notify_tray(TrayAction::Show);
    });

    Application::new().run(move |cx: &mut App| {
        wgpui_kit::init(cx);
        // The app lives in the tray with its window hidden; closing the
        // last window must never end the event loop on its own.
        cx.set_quit_mode(QuitMode::Explicit);
        // Built here so every platform constructs it on the event-loop
        // thread; the view owns it afterwards, and dropping removes it.
        let tray = match tray::build(tray_tx) {
            Ok(tray) => Some(tray),
            Err(error) => {
                log::error!(target: "tray", "tray build failed: {error}");
                None
            }
        };
        let params = WindowParams {
            ui_tx,
            shared: frame,
            gui_rx,
            tray,
            single,
        };
        let bounds = Bounds::centered(None, size(px(420.), px(520.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(420.), px(520.))),
                app_id: Some(APP_ID.to_owned()),
                ..Default::default()
            },
            move |window: &mut wgpui::Window, cx: &mut App| {
                window.set_window_title("Siphon");
                SiphonView::build(window, cx, params)
            },
        )
        .expect("siphon window");
        cx.activate(true);
    });
}
