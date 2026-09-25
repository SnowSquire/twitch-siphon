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

use wgpui::{App, Application, Bounds, QuitMode, WindowBounds, WindowOptions, px, size};

use crate::app::{SiphonView, WindowParams};
use crate::config::Config;
use crate::event_loop::EventLoop;
use crate::hermes::Session;
use crate::state::{
    FrameState, GuiSender, SharedFrame, UiIntent, UpdateStatus, WorkContext, WorkState,
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
    let tray_events = tray::spawn_proxy();
    let (gui_tx, gui_rx) = GuiSender::pair();
    // Latest-only snapshot slot shared by the work thread (writer) and the
    // GUI thread (reader); the sender beside every store wakes the GUI.
    let frame = SharedFrame::new(FrameState {
        config: config.clone(),
        status: None,
        error: String::new(),
        update: UpdateStatus::Idle,
    });

    //worker thread, runs hermes and event loop
    let work_frame = frame.clone();
    let wake_gui = gui_tx.clone();
    #[cfg(windows)]
    let theme_gui = gui_tx.clone();
    let _work_thread = std::thread::Builder::new()
        .name("worker thread".to_owned())
        .spawn(move || {
            // System theme notifications land here: no new thread, and COM
            // stays off the foreground thread's clipboard apartment.
            #[cfg(windows)]
            crate::app::watch_system_theme(theme_gui);
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
                                tray_events,
                                gui: gui_tx,
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

    // Second launches wake the window through the same gui channel the
    // foreground pump awaits, so showing works even while hidden.
    let single = app_single_instance::start_primary(APP_ID, move || {
        log::info!(target: "single", "wake signal received");
        wake_gui.notify_tray(TrayAction::Show);
    });

    Application::new().run(move |cx: &mut App| {
        wgpui_kit::init(cx);
        // The app lives in the tray with its window hidden; closing the
        // last window must never end the event loop on its own.
        cx.set_quit_mode(QuitMode::Explicit);
        // Built here so every platform constructs it on the event-loop
        // thread; the view owns it afterwards, and dropping removes it.
        let tray = match tray::build() {
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
