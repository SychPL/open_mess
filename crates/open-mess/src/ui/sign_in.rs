//! Sign-in prompts on their way from modules to the configuration window (spec 8.3, 8.8). Sign-in
//! starts only from the window, one at a time: a prompt from a module that is not signing in from
//! the window is refused. The window shows the one prompt that waits, and answers it; a newer
//! prompt, the end of the sign-in or the window closing cancel it. Once the user cancelled (or
//! closed the window), every later prompt of that sign-in is cancelled at once, so a module that
//! keeps asking runs into its call limit instead of holding the sign-in open.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use open_mess_host::{Prompt, SignInReply, SignInRequest};
use tokio::sync::{mpsc, oneshot, watch};

/// The answer to a prompt that does not come from a sign-in started in the window.
pub const NOT_FROM_WINDOW: &str = "logowanie można zacząć tylko w oknie konfiguracji";
/// Why a second sign-in is refused while one runs.
pub const BUSY: &str = "trwa już inne logowanie - dokończ je albo anuluj";
/// Why an answer is refused when its prompt no longer waits.
pub const GONE: &str = "ta prośba o logowanie już nie czeka";
/// Why the WAM dialog can't open.
pub const NO_WINDOW: &str = "okno konfiguracji nie jest otwarte";

/// The sign-in in progress and the prompt the window shows. Clones share it.
#[derive(Clone, Default)]
pub struct SignIns(Arc<Inner>);

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    /// Bumped whenever the shown prompt changes, for the window.
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    /// The module signing in from the window.
    module: Option<String>,
    shown: Option<Shown>,
    next_id: u64,
    /// The configuration window's handle while it is open: the owner of the WAM dialog.
    owner: Option<isize>,
    /// The user cancelled the sign-in in progress, or closed the window.
    cancelled: bool,
    /// A question about the shown prompt's sign-in page; it goes with the prompt.
    question: Option<Question>,
}

impl State {
    /// Takes the prompt off the screen, with its question.
    fn take_shown(&mut self) -> Option<Shown> {
        self.question = None;
        self.shown.take()
    }
}

/// May the sign-in page of the shown prompt go on to `host`? (spec 8.3, R10)
struct Question {
    id: u64,
    host: String,
    allow: oneshot::Sender<bool>,
}

/// The prompt on screen and where its answer goes.
struct Shown {
    id: u64,
    request: SignInRequest,
    reply: oneshot::Sender<SignInReply>,
}

impl Shown {
    fn cancel(self) {
        let _ = self.reply.send(SignInReply::Cancelled);
    }
}

/// The sign-in of one module from the window; dropping it ends the sign-in and cancels what still
/// waits.
pub struct Active(SignIns);

impl Drop for Active {
    fn drop(&mut self) {
        let shown = {
            let mut state = self.0.state();
            state.module = None;
            state.take_shown()
        };
        if let Some(shown) = shown {
            shown.cancel();
            self.0.changed();
        }
    }
}

impl SignIns {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self) {
        self.0.changed.send_modify(|count| *count = count.wrapping_add(1));
    }

    /// Changes whenever the prompt to show changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.changed.subscribe()
    }

    /// Starts the sign-in of `module`; refused while another one runs.
    pub fn begin(&self, module: &str) -> Result<Active, String> {
        let mut state = self.state();
        if state.module.is_some() {
            return Err(BUSY.into());
        }
        state.module = Some(module.into());
        state.cancelled = false;
        Ok(Active(self.clone()))
    }

    /// Takes the modules' prompts until every module context is gone.
    pub async fn forward(self, mut prompts: mpsc::Receiver<Prompt>) {
        while let Some((request, reply)) = prompts.recv().await {
            self.offer(request, reply);
        }
    }

    /// Shows a prompt, if its module is the one signing in; it replaces the one shown.
    pub fn offer(&self, request: SignInRequest, reply: oneshot::Sender<SignInReply>) {
        let replaced = {
            let mut state = self.state();
            if state.module.as_deref() != Some(request.module()) {
                let _ = reply.send(SignInReply::Refused(NOT_FROM_WINDOW.into()));
                return;
            }
            if state.cancelled {
                let _ = reply.send(SignInReply::Cancelled);
                return;
            }
            // Answered here: the user sees the WAM dialog itself.
            if let SignInRequest::WamWindow { .. } = request {
                let _ = reply.send(state.owner.map_or_else(|| SignInReply::Refused(NO_WINDOW.into()), SignInReply::Window));
                return;
            }
            state.next_id += 1;
            let id = state.next_id;
            let replaced = state.take_shown();
            state.shown = Some(Shown { id, request, reply });
            replaced
        };
        if let Some(old) = replaced {
            old.cancel();
        }
        self.changed();
    }

    /// The prompt to show, with the id its answer names.
    pub fn shown(&self) -> Option<(u64, SignInRequest)> {
        self.state().shown.as_ref().map(|shown| (shown.id, shown.request.clone()))
    }

    /// Asks the user whether the sign-in page of prompt `page` may go on to `host`; false when the
    /// user says no, or the prompt ends first.
    pub async fn ask_host(&self, page: u64, host: &str) -> bool {
        let allowed = {
            let mut state = self.state();
            if state.shown.as_ref().is_none_or(|shown| shown.id != page) {
                return false;
            }
            state.next_id += 1;
            let (allow, allowed) = oneshot::channel();
            state.question = Some(Question { id: state.next_id, host: host.into(), allow });
            allowed
        };
        self.changed();
        allowed.await.unwrap_or(false)
    }

    /// The question to show instead of the prompt, with the id its answer names, and the host.
    pub fn question(&self) -> Option<(u64, String)> {
        self.state().question.as_ref().map(|question| (question.id, question.host.clone()))
    }

    /// The user's answer to prompt or question `id`. A prompt goes off the screen, and a cancel
    /// cancels the rest of the sign-in too; a question is answered yes by anything but a cancel.
    pub fn answer(&self, id: u64, reply: SignInReply) -> Result<(), String> {
        let shown = {
            let mut state = self.state();
            if let Some(question) = state.question.take_if(|question| question.id == id) {
                drop(state);
                let _ = question.allow.send(reply != SignInReply::Cancelled);
                self.changed();
                return Ok(());
            }
            if state.shown.as_ref().is_none_or(|shown| shown.id != id) {
                return Err(GONE.into());
            }
            state.cancelled |= reply == SignInReply::Cancelled;
            match state.take_shown() {
                Some(shown) => shown,
                None => return Err(GONE.into()),
            }
        };
        let _ = shown.reply.send(reply);
        self.changed();
        Ok(())
    }

    /// The user refused what prompt `id` asked (a sign-in page's new host): the prompt ends refused,
    /// and the rest of the sign-in with it (spec 8.3, R10). A refusal for a technical reason is an
    /// ordinary [`SignIns::answer`], after which the module may try another way.
    pub fn refuse(&self, id: u64, why: String) -> Result<(), String> {
        self.answer(id, SignInReply::Refused(why))?;
        self.state().cancelled = true;
        Ok(())
    }

    /// The sign-in window of prompt `page` closed: if the prompt still waits, the user cancelled.
    pub fn page_closed(&self, page: u64) {
        let _ = self.answer(page, SignInReply::Cancelled);
    }

    /// The configuration window opened, with this handle (`HWND`).
    pub fn window_opened(&self, owner: isize) {
        self.state().owner = Some(owner);
    }

    /// The window closed: what it showed is cancelled.
    pub fn window_closed(&self) {
        let shown = {
            let mut state = self.state();
            state.owner = None;
            state.cancelled = state.module.is_some();
            state.take_shown()
        };
        if let Some(shown) = shown {
            shown.cancel();
            self.changed();
        }
    }
}
