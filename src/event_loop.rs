//! UI intent pump. Awaits UI intents, tray events, and channel resolve
//! completions; the runtime otherwise sleeps. Intent handling is
//! synchronous; resolves wait in a `FuturesUnordered` polled by this same
//! routine, so no task is ever spawned.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt as _, StreamExt as _};

use crate::gql;
use crate::state::{UiIntent, WorkState};

/// One finished channel resolve: the login plus the gql result.
type ChannelResolveDone = (String, Result<Vec<gql::User>, gql::Error>);
/// In-flight resolve future. `!Send` is fine: everything stays on this one
/// thread-per-core runtime thread.
type ChannelFuture = Pin<Box<dyn Future<Output = ChannelResolveDone>>>;

pub struct EventLoop {
    work: Rc<RefCell<WorkState>>,
    pending: FuturesUnordered<ChannelFuture>,
}

impl EventLoop {
    pub fn new(work: Rc<RefCell<WorkState>>) -> Self {
        Self {
            work,
            pending: FuturesUnordered::new(),
        }
    }

    pub async fn run(&mut self) {
        let ui_rx = self.work.borrow().ui_rx.clone();
        let tray_rx = self.work.borrow().tray_events.clone();
        loop {
            if self.pending.is_empty() {
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
                }
            } else {
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
                    done = self.pending.next().fuse() => {
                        if let Some((login, result)) = done {
                            WorkState::finish_add_login(&self.work, login, result).await;
                        }
                    }
                }
            }
        }
    }

    async fn handle_intent(&self, intent: UiIntent) {
        match intent {
            UiIntent::AddLogin(login) => {
                if let Some(login) = self.work.borrow_mut().begin_add_login(&login) {
                    self.pending.push(Box::pin(async move {
                        let users = gql::fetch_users(&[], std::slice::from_ref(&login)).await;
                        (login, users)
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
            UiIntent::ClearError => self.work.borrow_mut().clear_error(),
        }
    }
}
