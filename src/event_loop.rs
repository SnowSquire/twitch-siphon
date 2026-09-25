use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt as _, StreamExt as _};

use crate::http;
use crate::notifier::{self, ToastJob};
use crate::state::{UiIntent, WorkState};
use crate::update;

/// One finished background job: a channel resolve, an update step, or a
/// shown toast. All kinds share the queue; completions dispatch in
/// [`EventLoop::handle_job`].
enum JobDone {
    ChannelResolved(String, anyhow::Result<Option<http::User>>),
    UpdateCheckFinished,
    UpdateCheckDue,
    UpdateDownloadFinished(anyhow::Result<PathBuf>),
    ToastShown,
}
/// One in-flight job. `!Send` is fine: everything stays on this one
/// thread-per-core runtime thread.
type JobFuture = Pin<Box<dyn Future<Output = JobDone>>>;

pub struct EventLoop {
    work: Rc<RefCell<WorkState>>,
    toast_rx: kanal::AsyncReceiver<ToastJob>,
    jobs: FuturesUnordered<JobFuture>,
}

impl EventLoop {
    pub fn new(work: Rc<RefCell<WorkState>>, toast_rx: kanal::AsyncReceiver<ToastJob>) -> Self {
        let this = Self {
            work: Rc::clone(&work),
            toast_rx,
            jobs: FuturesUnordered::new(),
        };
        // Check immediately at startup; completions re-arm the interval.
        this.push_update_check();
        this
    }

    /// Queues one release check.
    fn push_update_check(&self) {
        let work = Rc::clone(&self.work);
        self.jobs.push(Box::pin(async move {
            WorkState::check_for_update(&work).await;
            JobDone::UpdateCheckFinished
        }));
    }

    /// Queues the wait after a check; expiry queues the next check.
    fn push_update_wait(&self) {
        self.jobs.push(Box::pin(async move {
            compio::time::sleep(update::CHECK_INTERVAL).await;
            JobDone::UpdateCheckDue
        }));
    }

    /// Queues one toast for display. Showing parks only this job;
    /// intents and session traffic keep flowing through the loop.
    fn push_toast(&self, job: ToastJob) {
        self.jobs.push(Box::pin(async move {
            notifier::display(job);
            JobDone::ToastShown
        }));
    }

    pub async fn run(&mut self) {
        let ui_rx = self.work.borrow().ui_rx.clone();
        let tray_rx = self.work.borrow().tray_events.clone();
        let toast_rx = self.toast_rx.clone();
        loop {
            futures_util::select! {
                intent = ui_rx.recv().fuse() => {
                    match intent {
                        Ok(intent) => self.handle_intent(intent).await,
                        Err(_) => break,
                    }
                }
                action = tray_rx.recv().fuse() => {
                    match action {
                        Ok(action) => self.work.borrow().forward_tray(action),
                        Err(_) => break,
                    }
                }
                toast = toast_rx.recv().fuse() => {
                    match toast {
                        Ok(job) => self.push_toast(job),
                        Err(_) => break,
                    }
                }
                done = self.next_job().fuse() => {
                    if let Some(done) = done {
                        self.handle_job(done).await;
                    }
                }
            }
        }
    }

    /// Next queued job completion. An empty queue resolves immediately,
    /// which would busy-loop the `select!` above, so the empty case pends
    /// instead until a job is queued.
    async fn next_job(&mut self) -> Option<JobDone> {
        if self.jobs.is_empty() {
            std::future::pending::<()>().await;
            None
        } else {
            self.jobs.next().await
        }
    }

    async fn handle_job(&self, done: JobDone) {
        match done {
            JobDone::ChannelResolved(login, result) => {
                WorkState::finish_add_login(&self.work, login, result).await;
            }
            JobDone::UpdateCheckFinished => self.push_update_wait(),
            JobDone::UpdateCheckDue => self.push_update_check(),
            JobDone::UpdateDownloadFinished(result) => {
                self.work.borrow_mut().finish_update_download(result);
            }
            JobDone::ToastShown => {}
        }
    }

    async fn handle_intent(&self, intent: UiIntent) {
        match intent {
            UiIntent::AddLogin(login) => {
                if let Some(login) = self.work.borrow_mut().begin_add_login(&login) {
                    self.jobs.push(Box::pin(async move {
                        let user = http::fetch_user(&login).await;
                        JobDone::ChannelResolved(login, user)
                    }));
                }
            }
            UiIntent::RemoveChannel(id) => {
                WorkState::apply_remove_channel(&self.work, id).await;
            }
            UiIntent::SetNotifyTitleChanges(value) => {
                WorkState::apply_notify_title_changes(&self.work, value).await;
            }
            UiIntent::SetSound(value) => {
                WorkState::apply_sound(&self.work, value).await;
            }
            UiIntent::AddFilteredWord(word) => {
                WorkState::apply_add_filtered_word(&self.work, word).await;
            }
            UiIntent::RemoveFilteredWord(index) => {
                WorkState::apply_remove_filtered_word(&self.work, index).await;
            }
            UiIntent::ApplyUpdate => {
                let Some(offer) = self.work.borrow_mut().begin_update() else {
                    return;
                };
                // MSI installs download the installer; everything else
                // opens the release page right away, keeping the offer.
                let msi = matches!(update::install_mode(), update::InstallMode::Msi)
                    .then(|| offer.msi_url.clone())
                    .flatten();

                if let Some(url) = msi {
                    let version = offer.version.clone();
                    self.jobs.push(Box::pin(async move {
                        JobDone::UpdateDownloadFinished(update::download_msi(&url, &version).await)
                    }));
                } else {
                    self.work.borrow_mut().finish_update_page(&offer);
                }
            }
            UiIntent::ClearError => self.work.borrow_mut().clear_error(),
        }
    }
}
