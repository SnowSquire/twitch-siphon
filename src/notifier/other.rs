use notify_rust::{Notification, NotificationResponse, Timeout};

use super::ToastJob;

/// Shows one toast and, when it carries a login, waits for the click.
/// `Never` keeps the banner up and leaves it in the notification history.
/// The wait ends on popup dismissal — this backend only delivers the first
/// event, so history clicks afterwards are missed.
pub(super) fn show_toast(job: &ToastJob) {
    let mut notification = Notification::new();
    let builder = notification
        .summary(&job.summary)
        .body(&job.body)
        .timeout(Timeout::Never)
        .sound_name(if job.sound { "Default" } else { "Silent" })
        .action("open", "Watch");
    if let Some(image) = &job.image {
        builder.image_path(image);
    }
    let handle = match builder.show() {
        Ok(handle) => handle,
        Err(error) => {
            log::info!(target: "notifier", "failed to show notification: {error}");
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
            if let Err(error) = open::that(&url) {
                log::info!(target: "notifier", "failed to open {url}: {error}");
            }
        }
    }) {
        log::info!(target: "notifier", "notification response failed: {error}");
    }
}
