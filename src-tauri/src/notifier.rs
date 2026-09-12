#[cfg(target_os = "windows")]
use tauri_winrt_notification::{Duration, Sound, Toast};
#[cfg(not(target_os = "windows"))]
use notify_rust::{Notification, NotificationResponse, Timeout};

use crate::gql;
use crate::logging::log;

/// aumid the toasts are shown under; registered per-user on startup so the
/// notification banner shows us instead of the powershell fallback
#[cfg(target_os = "windows")]
const AUMID: &str = "twitch-siphon";
#[cfg(target_os = "windows")]
const DISPLAY_NAME: &str = "Siphon";

/// registers `HKCU\Software\Classes\AppUserModelId\{AUMID}` with our display
/// name and an icon unpacked to the temp dir; re-running every startup is
/// cheap and self-heals a deleted icon
#[cfg(target_os = "windows")]
pub fn register_aumid() {
    let icon_path = std::env::temp_dir().join("twitch-siphon-icon.png");
    if let Err(error) = std::fs::write(&icon_path, include_bytes!("../icons/icon.png")) {
        log(
            "notifier",
            format!("failed to write notification icon: {error}"),
        );
        return;
    }
    let result = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .create_subkey(format!("Software\\Classes\\AppUserModelId\\{AUMID}"))
        .and_then(|(key, _)| {
            key.set_value("DisplayName", &DISPLAY_NAME)?;
            key.set_value("IconUri", &icon_path.as_os_str())
        });
    if let Err(error) = result {
        log("notifier", format!("failed to register aumid: {error}"));
    }
}

/// One toast request; everything owned so it can cross to the worker thread.
struct ToastJob {
    summary: String,
    body: String,
    sound: bool,
    image: Option<String>,
    login: Option<String>,
}

/// The single notification worker's inbox, spawned on first use. The channel
/// is unbounded so `notify` never blocks; the worker shows toasts strictly
/// in order.
fn toast_sender() -> &'static std::sync::mpsc::Sender<ToastJob> {
    static INBOX: std::sync::OnceLock<std::sync::mpsc::Sender<ToastJob>> =
        std::sync::OnceLock::new();
    INBOX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<ToastJob>();
        std::thread::spawn(move || {
            for job in rx {
                show_toast(&job);
            }
        });
        tx
    })
}

/// Shows one toast. The click opens the url directly from the activation
/// callback, so there is nothing to wait for: `show()` returns once the
/// toast is handed off, and history clicks keep working for as long as the
/// process lives. `Long` duration leaves the toast in the Action Center.
#[cfg(target_os = "windows")]
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
            if let Err(error) = tauri_plugin_opener::open_url(&url, None::<&str>) {
                log("notifier", format!("failed to open {url}: {error}"));
            }
            Ok(())
        });
    }
    let result = toast.show();
    if let Err(error) = result {
        log("notifier", format!("failed to show notification: {error}"));
    }
}

/// Shows one toast and, when it carries a login, waits for the click.
/// `Never` keeps the banner up and leaves it in the notification history.
/// The wait ends on popup dismissal — this backend only delivers the first
/// event, so history clicks afterwards are missed.
#[cfg(not(target_os = "windows"))]
fn show_toast(job: &ToastJob) {
    let mut notification = Notification::new();
    let builder = notification
        .summary(&job.summary)
        .body(&job.body)
        .timeout(Timeout::Never)
        .sound_name(if job.sound {
            "Default"
        } else {
            "Silent"
        })
        .action("open", "Watch");
    if let Some(image) = &job.image {
        builder.image_path(image);
    }
    let handle = match builder.show() {
        Ok(handle) => handle,
        Err(error) => {
            log("notifier", format!("failed to show notification: {error}"));
            return;
        }
    };
    let Some(login) = &job.login else {
        return;
    };
    let url = format!("https://www.twitch.tv/{login}");
    if let Err(error) = handle.wait_for_response(|response: &NotificationResponse| {
        if matches!(
            response,
            NotificationResponse::Default | NotificationResponse::Action(_)
        ) {
            if let Err(error) = tauri_plugin_opener::open_url(&url, None::<&str>) {
                log("notifier", format!("failed to open {url}: {error}"));
            }
        }
    }) {
        log("notifier", format!("notification response failed: {error}"));
    }
}

/// downloads the avatar into a per-url temp cache file and returns the local
/// path; the cdn url embeds the avatar hash, so a changed picture lands in a
/// new file and stale ones are simply abandoned
async fn resolve_image(image: Option<&str>) -> Option<String> {
    let url = image?;
    let file_name = url.rsplit('/').next().filter(|name| !name.is_empty())?;
    let dir = std::env::temp_dir().join("twitch-siphon-avatars");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(file_name);
    if !path.exists() {
        if let Err(error) = gql::fetch_file(url, &path).await {
            log("notifier", format!("failed to download avatar: {error}"));
            return None;
        }
    }
    // notify-rust takes paths as &str; bail out of impossibly-named temp dirs
    path.into_os_string().into_string().ok()
}

/// "Silent" intentionally fails `Sound::from_str(..)`; notify-rust maps the
/// failed parse to no sound at all, which is exactly the silent toast.
///
/// When `login` is `Some`, clicking the toast body (or the Watch button)
/// opens that channel in the default browser. Only the login is stored; the
/// url is built at click time. Toasts never expire on their own: they use the
/// long duration and persist in the Action Center / history.
pub async fn notify(
    summary: &str,
    body: &str,
    sound: bool,
    image: Option<&str>,
    login: Option<&str>,
) {
    log(
        "notifier",
        format!("showing notification: {summary} | {body}"),
    );
    // One worker owns all toasts: showing blocks, so a thread per toast
    // would pile up pool threads under bursts. Showing and waiting also must
    // happen on the same thread — some backends' handles are `!Send`.
    // Sequential display is fine at our rate and keeps order.
    let job = ToastJob {
        summary: summary.to_owned(),
        body: body.to_owned(),
        sound,
        image: resolve_image(image).await,
        login: login.map(str::to_owned),
    };
    if toast_sender().send(job).is_err() {
        log("notifier", "notification worker gone");
    }
}
