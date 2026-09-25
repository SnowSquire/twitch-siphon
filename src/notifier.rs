//! WinRT toast notifications. Toasts are queued by [`notify`] and shown
//! as jobs on the event loop ([`display`]); there is no helper thread.
//! Clicks open the channel from the activation callback, so showing never
//! waits.

use kanal::Sender;
use tauri_winrt_notification::{Duration, Sound, Toast};

use crate::http;

/// aumid the toasts are shown under; registered per-user on startup so the
/// notification banner shows us instead of the powershell fallback
const AUMID: &str = "twitch-siphon";
const DISPLAY_NAME: &str = "Siphon";

/// One toast request; everything owned so the job can wait in the
/// event-loop queue past the end of the `notify` call that enqueued it.
pub(crate) struct ToastJob {
    summary: String,
    body: String,
    sound: bool,
    image: Option<String>,
    login: Option<String>,
}

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

/// downloads the avatar into a per-url temp cache file and returns the local
/// path; the cdn url embeds the avatar hash, so a changed picture lands in a
/// new file and stale ones are simply abandoned. Async fs throughout: this
/// runs on the thread-per-core runtime, where synchronous file IO would block.
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

/// Shows one toast. The click opens the url directly from the activation
/// callback, so there is nothing to wait for: `show()` returns once the
/// toast is handed off, and history clicks keep working for as long as the
/// process lives. `Long` duration leaves the toast in the Action Center.
fn show_toast(job: &ToastJob) {
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

/// Shows one queued toast as an event-loop job, so a burst of toasts never
/// parks the session or the intent pump.
pub(crate) fn display(job: ToastJob) {
    show_toast(&job);
}

/// Queues a toast for the event loop to show. The avatar resolves first so
/// the queued job is fully owned; the send never blocks, so hermes never
/// stalls on a popup the user hasn't dismissed. Toasts never expire on their
/// own: they use the long duration and persist in the Action Center / history.
pub async fn notify(
    toast_tx: &Sender<ToastJob>,
    summary: &str,
    body: &str,
    sound: bool,
    image: Option<&str>,
    login: Option<&str>,
) {
    log::info!(target: "notifier", "showing notification: {summary} | {body}");
    let job = ToastJob {
        summary: summary.to_owned(),
        body: body.to_owned(),
        sound,
        image: resolve_image(image).await,
        login: login.map(str::to_owned),
    };
    if toast_tx.send(job).is_err() {
        log::info!(target: "notifier", "event loop gone, dropping toast");
    }
}
