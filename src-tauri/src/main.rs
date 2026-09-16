// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod gql;
mod hermes;
mod logging;
mod notifier;
mod state;
mod tray;

use std::sync::Arc;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::config::Config;
use crate::state::{FrameState, GuiWaker, UiIntent, WorkContext};
use crate::tray::TrayAction;

/// Former Tauri bundle id; doubles as the single-instance key and the
/// config directory name so existing installs keep their config.
const APP_ID: &str = "com.iken.siphon";

fn main() {
    env_logger::init();

    // A second launch wakes the first instance and exits immediately.
    if app_single_instance::notify_if_running(APP_ID) {
        return;
    }

    #[cfg(target_os = "windows")]
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

    // Two threads, three channels, one doorbell. The GUI thread renders and
    // sends intents; the work thread owns all state and pushes frames back.
    // Dropping the JoinHandle detaches the work thread; process exit cleans
    // it up.
    let (ui_tx, ui_rx) = crossbeam_channel::unbounded::<UiIntent>();
    let (frame_tx, frame_rx) = crossbeam_channel::unbounded::<FrameState>();
    let (tray_tx, tray_rx) = crossbeam_channel::unbounded::<TrayAction>();
    let waker = GuiWaker::new();
    let initial = FrameState {
        config: config.clone(),
        status: None,
        error: String::new(),
    };
    let _work_thread = hermes::spawn(WorkContext {
        config_path,
        config,
        ui_rx,
        frame_tx,
        tray_tx,
        waker: waker.clone(),
    });

    // Primary-instance listener, kept alive until the process exits. The
    // callback only wakes the event loop; the `check_show` poll in `logic()`
    // performs the actual show (the context may not exist yet here).
    let wake = waker.clone();
    let single = app_single_instance::start_primary(APP_ID, move || {
        log::info!(target: "single", "wake signal received, waking event loop");
        wake.repaint();
    });

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Siphon")
        .with_app_id(APP_ID)
        .with_inner_size([420.0, 520.0]);

    match tray::load_icon_rgba() {
        Ok((rgba, width, height)) => {
            viewport = viewport.with_icon(Arc::new(egui::IconData {
                rgba,
                width,
                height,
            }));
        }
        Err(error) => log::info!(target: "app", "window icon unavailable: {error}"),
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let run_result = eframe::run_native(
        "Siphon",
        options,
        Box::new(move |cc| {
            // Raw Win32 handle for ShowWindow hide/show (emilk/egui#737);
            // `None` off Windows, where viewport commands suffice.
            let hwnd = cc
                .window_handle()
                .ok()
                .and_then(|handle| match handle.as_raw() {
                    RawWindowHandle::Win32(window) => Some(window.hwnd.get()),
                    _ => None,
                });
            log::info!(target: "app", "creator closure running, hwnd={hwnd:?}");
            // No tray (failed build) means close quits instead of hiding.
            let tray = match tray::build() {
                Ok(tray) => Some(tray),
                Err(error) => {
                    log::error!(target: "tray", "tray build failed: {error}");
                    None
                }
            };
            waker.set(cc.egui_ctx.clone());
            Ok(Box::new(app::SiphonApp::new(
                ui_tx, frame_rx, tray_rx, initial, tray, single, hwnd,
            )) as Box<dyn eframe::App>)
        }),
    );
    if let Err(error) = run_result {
        log::info!(target: "app", "eframe exited with error: {error}");
    }
}
