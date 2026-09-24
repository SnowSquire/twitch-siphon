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
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use eframe::egui_wgpu;
#[cfg(windows)]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::config::Config;
use crate::event_loop::EventLoop;
use crate::hermes::Session;
use crate::state::{
    FrameState, GuiWaker, SharedFrame, UiIntent, UpdateStatus, WorkContext, WorkState,
};
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

    let (ui_tx, ui_rx) = kanal::unbounded::<UiIntent>();
    let (tray_tx, tray_rx) = kanal::unbounded::<TrayAction>();
    let tray_events = tray::spawn_proxy();
    let waker = GuiWaker::new();
    // Latest-only snapshot slot shared by the work thread (writer) and the
    // GUI thread (reader); the waker poke beside every store wakes the GUI.
    let frame = SharedFrame::new(FrameState {
        config: config.clone(),
        status: None,
        error: String::new(),
        update: UpdateStatus::Idle,
    });

    //worker thread, runs hermes and event loop
    let work_waker = waker.clone();
    let work_frame = frame.clone();
    let _work_thread = std::thread::Builder::new()
        .name("worker thread".to_owned())
        .spawn(move || {
            {
                compio::runtime::Runtime::new()
                    .expect("compio runtime")
                    .block_on(async move {
                        let (session_tx, command_rx) = kanal::unbounded();
                        let work = Rc::new(RefCell::new(WorkState::new(
                            WorkContext {
                                config_path,
                                config,
                                ui_rx,
                                frame: work_frame,
                                tray_tx,
                                tray_events,
                                waker: work_waker,
                            },
                            session_tx,
                        )));
                        work.borrow().push_frame();
                        let mut hermes = Session::new(Rc::clone(&work), command_rx.to_async());
                        let mut events = EventLoop::new(work);

                        futures_util::future::join(events.run(), hermes.run()).await;
                    });
            };
        })
        .expect("hermes thread");

    // Spawns a thread
    let wake = waker.clone();
    let single = app_single_instance::start_primary(APP_ID, move || {
        log::info!(target: "single", "wake signal received, waking event loop");
        wake.repaint();
    });

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Siphon")
        .with_app_id(APP_ID)
        .with_inner_size([420.0, 520.0])
        .with_min_inner_size([420.0, 520.0]);

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

    // Non-blocking presents. The Fifo/AutoVsync path stalls on the Windows
    // flip path during a resize storm: the GUI thread keeps running but the
    // compositor stops showing new frames, freezing the window. Mailbox is
    // non-blocking like Immediate but paced to the display and tear-free.
    // One frame in flight keeps the backlog bounded.

    let run_result = eframe::run_native(
        "Siphon",
        eframe::NativeOptions {
            viewport,
            wgpu_options: egui_wgpu::WgpuConfiguration {
                surface: eframe::SurfaceConfig {
                    present_mode: wgpu::PresentMode::Mailbox,
                    desired_maximum_frame_latency: Some(1),
                },
                ..Default::default()
            },
            ..Default::default()
        },
        Box::new(move |cc| {
            #[cfg(windows)]
            let hwnd = cc
                .window_handle()
                .ok()
                .and_then(|handle| match handle.as_raw() {
                    RawWindowHandle::Win32(window) => Some(window.hwnd.get()),
                    _ => None,
                })
                .ok_or("missing Win32 window handle")?;
            let tray = match tray::build() {
                Ok(tray) => Some(tray),
                Err(error) => {
                    log::error!(target: "tray", "tray build failed: {error}");
                    None
                }
            };
            waker.set(cc.egui_ctx.clone());
            #[cfg(windows)]
            let app = app::SiphonApp::new(ui_tx, frame, tray_rx, tray, single, hwnd);
            #[cfg(not(windows))]
            let app = app::SiphonApp::new(ui_tx, frame, tray_rx, tray, single);

            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    );
    if let Err(error) = run_result {
        log::info!(target: "app", "eframe exited with error: {error}");
    }
}
