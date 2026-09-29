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
mod ui;
mod update;

use slint::ComponentHandle as _;

use crate::app::{ViewParams, build};
use crate::config::Config;
use crate::state::{SharedSnapshot, Snapshot, UiIntent, Worker, WorkerInit};
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
    let (snapshot, gui_rx) = SharedSnapshot::pair(Snapshot {
        config: config.clone(),
        ..Snapshot::default()
    });

    // One handle per consumer moved into the thread below: worker,
    // single-instance wake callback, theme watcher.
    let work_frame = snapshot.clone();
    let wake_frame = snapshot.clone();
    let theme_frame = snapshot.clone();
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
                        let mut worker = Worker::new(WorkerInit {
                            config_path,
                            config,
                            ui_rx,
                            tray_rx,
                            shared: work_frame,
                        });

                        worker.shared.update(|_| {});

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

    // Built here so the tray message window lives on the event-loop thread;
    // the guards own it afterwards, and dropping removes the icon.
    let tray = match tray::build(tray_tx) {
        Ok(tray) => Some(tray),
        Err(error) => {
            log::error!(target: "tray", "tray build failed: {error}");
            None
        }
    };
    let (ui, guards) = build(ViewParams {
        ui_tx,
        shared: snapshot,
        gui_rx,
        tray,
        single,
    });
    ui.run().expect("slint event loop");
    drop(guards);
}
