use tauri_winrt_notification::{Duration, Sound, Toast};

use crate::http;

/// aumid the toasts are shown under; registered per-user on startup so the
/// notification banner shows us instead of the powershell fallback
const AUMID: &str = "twitch-siphon";
const DISPLAY_NAME: &str = "Siphon";

pub(crate) async fn show(
    summary: String,
    body: String,
    sound: bool,
    image: Option<String>,
    login: Option<String>,
) {
    log::info!(target: "notifier", "showing notification: {summary} | {body}");
    show_toast(
        &summary,
        &body,
        sound,
        resolve_image(image.as_deref()).await,
        login,
    );
}

pub fn register_aumid() {
    let icon_path = std::env::temp_dir().join("twitch-siphon-icon.png");
    if let Err(error) = std::fs::write(&icon_path, crate::tray::ICON_PNG) {
        log::info!(target: "notifier", "failed to write notification icon: {error}");
        return;
    }
    let result = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .create_subkey(format!("Software\\Classes\\AppUserModelId\\{AUMID}"))
        .and_then(|(key, _)| {
            key.set_value("DisplayName", &DISPLAY_NAME)?;
            key.set_value("IconUri", &icon_path.as_os_str())
        });
    if let Err(error) = result {
        log::info!(target: "notifier", "failed to register aumid: {error}");
    }
}

async fn resolve_image(image: Option<&str>) -> Option<String> {
    let url = image?;
    let file_name = url.rsplit('/').next().filter(|name| !name.is_empty())?;
    let dir = std::env::temp_dir().join("twitch-siphon-avatars");
    compio::fs::create_dir_all(&dir).await.ok()?;
    let path = dir.join(file_name);
    if compio::fs::metadata(&path).await.is_err()
        && let Err(error) = http::fetch_file(url, &path).await
    {
        log::info!(target: "notifier", "failed to download avatar: {error}");
        return None;
    }
    path.into_os_string().into_string().ok()
}

fn show_toast(
    summary: &str,
    body: &str,
    sound: bool,
    image: Option<String>,
    login: Option<String>,
) {
    let mut toast = Toast::new(AUMID)
        .title(summary)
        .text2(body)
        .duration(Duration::Long)
        .sound(if sound { Some(Sound::Default) } else { None });
    if login.is_some() {
        toast = toast.add_button("Watch", "open");
    }
    if let Some(image) = &image {
        toast = toast.image(std::path::Path::new(image), "");
    }
    if let Some(login) = login {
        toast = toast.on_activated(move |_| {
            let url = format!("https://www.twitch.tv/{login}");
            if let Err(error) = open::that(&url) {
                log::info!(target: "notifier", "failed to open {url}: {error}");
            }
            Ok(())
        });
    }
    let result = toast.show();
    if let Err(error) = result {
        log::info!(target: "notifier", "failed to show notification: {error}");
    }
}
