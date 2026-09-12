mod config;
mod gql;
mod hermes;
mod logging;
mod notifier;

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, State, WindowEvent};

use crate::config::{Channel, Config};
use crate::hermes::ChannelEntry;
use crate::logging::log;

struct AppState {
    config_path: PathBuf,
    // the in-memory config is the source of truth; every mutation command
    // updates it, persists it to disk and forwards a hermes command
    config: Mutex<Config>,
    hermes_tx: tokio::sync::mpsc::UnboundedSender<hermes::Command>,
    status: Mutex<Option<hermes::StatusEvent>>,
}

#[tauri::command]
async fn get_config(state: State<'_, AppState>) -> Result<Config, ()> {
    Ok(state.config.lock().expect("config lock poisoned").clone())
}

#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<Option<hermes::StatusEvent>, ()> {
    Ok(state.status.lock().expect("status lock poisoned").clone())
}

#[tauri::command]
async fn add_channel(state: State<'_, AppState>, login: String) -> Result<(), String> {
    let login = login.trim().to_lowercase();
    if state
        .config
        .lock()
        .expect("config lock poisoned")
        .channels
        .iter()
        .any(|channel| channel.login.eq_ignore_ascii_case(&login))
    {
        return Ok(());
    }
    // resolve before storing: only resolved channels hit the disk. A login
    // that fails to resolve is forwarded for the session only, so the ui
    // still shows it as not found without persisting the typo.
    let resolved = match gql::fetch_users(&[], std::slice::from_ref(&login)).await {
        Ok(users) => users
            .into_iter()
            .find(|user| user.channel_name.eq_ignore_ascii_case(&login))
            .map(|user| {
                log(
                    "config",
                    format!(
                        "resolved {} to id {} ({})",
                        login, user.channel_id, user.channel_display_name
                    ),
                );
                Channel {
                    login: user.channel_name,
                    id: user.channel_id,
                    display_name: Some(user.channel_display_name),
                }
            }),
        Err(error) => {
            log("config", format!("channel resolution failed: {error}"));
            None
        }
    };
    if let Some(channel) = resolved {
        let entry = ChannelEntry {
            login: channel.login.clone(),
            id: Some(channel.id),
            display_name: channel.display_name.clone(),
        };
        let mut config = state.config.lock().expect("config lock poisoned").clone();
        if config
            .channels
            .iter()
            .any(|existing| existing.id == channel.id)
        {
            return Ok(());
        }
        config.channels.push(channel);
        state.persist(&config)?;
        state
            .hermes_tx
            .send(hermes::Command::AddChannel(entry))
            .map_err(|error| error.to_string())
    } else {
        log(
            "config",
            format!("gql returned no channel named {login}, is it a typo?"),
        );
        state
            .hermes_tx
            .send(hermes::Command::AddChannel(ChannelEntry {
                login,
                id: None,
                display_name: None,
            }))
            .map_err(|error| error.to_string())
    }
}

#[tauri::command]
async fn remove_channel(state: State<'_, AppState>, login: String) -> Result<(), String> {
    let mut config = state.config.lock().expect("config lock poisoned").clone();
    config
        .channels
        .retain(|channel| !channel.login.eq_ignore_ascii_case(&login));
    state.persist(&config)?;
    state
        .hermes_tx
        .send(hermes::Command::RemoveChannel(login))
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn set_notify_title_changes(state: State<'_, AppState>, value: bool) -> Result<(), String> {
    let mut config = state.config.lock().expect("config lock poisoned").clone();
    config.notify_title_changes = value;
    state.persist(&config)?;
    state
        .hermes_tx
        .send(hermes::Command::SetNotifyTitleChanges(value))
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn set_sound(state: State<'_, AppState>, value: bool) -> Result<(), String> {
    let mut config = state.config.lock().expect("config lock poisoned").clone();
    config.sound = value;
    state.persist(&config)?;
    state
        .hermes_tx
        .send(hermes::Command::SetSound(value))
        .map_err(|error| error.to_string())
}

impl AppState {
    fn persist(&self, config: &Config) -> Result<(), String> {
        let mut stamped = config.clone();
        stamped.version = Config::VERSION;
        *self.config.lock().expect("config lock poisoned") = stamped.clone();
        stamped
            .save(&self.config_path)
            .map_err(|error| error.to_string())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // a second instance was launched: focus the existing window instead
            show_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_status,
            add_channel,
            remove_channel,
            set_notify_title_changes,
            set_sound
        ])
        .setup(|app| {
            // the aumid must exist before the first toast goes out
            #[cfg(target_os = "windows")]
            notifier::register_aumid();
            let config_path = app.path().app_config_dir()?.join("config.json");
            let mut config = Config::load(&config_path);
            log(
                "config",
                format!(
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
                ),
            );
            // stamp pre-version files so every write carries the version
            if config.version != Config::VERSION {
                config.version = Config::VERSION;
                config
                    .save(&config_path)
                    .map_err(|error| error.to_string())?;
            }

            let (hermes_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
            app.manage(AppState {
                config_path,
                config: Mutex::new(config.clone()),
                hermes_tx: hermes_tx.clone(),
                status: Mutex::new(None),
            });
            hermes::spawn(app.handle().clone(), command_rx);
            for channel in &config.channels {
                hermes_tx
                    .send(hermes::Command::AddChannel(ChannelEntry {
                        login: channel.login.clone(),
                        id: Some(channel.id),
                        display_name: channel.display_name.clone(),
                    }))
                    .map_err(|error| error.to_string())?;
            }
            hermes_tx
                .send(hermes::Command::SetNotifyTitleChanges(
                    config.notify_title_changes,
                ))
                .map_err(|error| error.to_string())?;
            hermes_tx
                .send(hermes::Command::SetSound(config.sound))
                .map_err(|error| error.to_string())?;

            let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(
                    app.default_window_icon()
                        .expect("default window icon")
                        .clone(),
                )
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // closing the window keeps the app running in the tray
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
