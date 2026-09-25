use tauri_winrt_notification::{Duration, Sound, Toast};

use super::ToastJob;

/// aumid the toasts are shown under; registered per-user on startup so the
/// notification banner shows us instead of the powershell fallback
const AUMID: &str = "twitch-siphon";
const DISPLAY_NAME: &str = "Siphon";

/// registers `HKCU\Software\Classes\AppUserModelId\{AUMID}` with our display
/// name and an icon unpacked to the temp dir; re-running every startup is
/// cheap and self-heals a deleted icon
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

/// Shows one toast. The click opens the url directly from the activation
/// callback, so there is nothing to wait for: `show()` returns once the
/// toast is handed off, and history clicks keep working for as long as the
/// process lives. `Long` duration leaves the toast in the Action Center.
pub(super) fn show_toast(job: &ToastJob) {
    let mut toast = Toast::new(AUMID)
        .title(&job.summary)
        .text2(&job.body)
        .duration(Duration::Long)
        .sound(if job.sound {
            Some(Sound::Default)
        } else {
            None
        });
    if job.login.is_some() {
        toast = toast.add_button("Watch", "open");
    }
    if let Some(image) = &job.image {
        toast = toast.image(std::path::Path::new(image), "");
    }
    if let Some(login) = job.login.clone() {
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
