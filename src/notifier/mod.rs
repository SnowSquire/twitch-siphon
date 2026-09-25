#[cfg(windows)]
mod windows;
#[cfg(not(windows))]
mod other;

#[cfg(windows)]
pub use windows::register_aumid;
#[cfg(windows)]
use windows::show_toast;
#[cfg(not(windows))]
use other::show_toast;

use crate::http;

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
    log::info!(target: "notifier", "showing notification: {summary} | {body}");
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
        log::info!(target: "notifier", "notification worker gone");
    }
}
