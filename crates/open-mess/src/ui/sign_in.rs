//! Sign-in prompts on their way from modules to the configuration window (spec 8.3, 8.8). Sign-in
//! starts only from the window, one at a time: a prompt from a module that is not signing in from
//! the window is refused. The window shows the one prompt that waits, and answers it; a newer
//! prompt, the end of the sign-in or the window closing cancel it.

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
}

/// The prompt on screen; `reply` is None once nothing waits for its answer.
struct Shown {
    id: u64,
    request: SignInRequest,
    reply: Option<oneshot::Sender<SignInReply>>,
}

impl Shown {
    fn cancel(self) {
        if let Some(reply) = self.reply {
            let _ = reply.send(SignInReply::Cancelled);
        }
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
            state.shown.take()
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
            // Answered here: the user sees the WAM dialog itself.
            if let SignInRequest::WamWindow { .. } = request {
                let _ = reply.send(state.owner.map_or_else(|| SignInReply::Refused(NO_WINDOW.into()), SignInReply::Window));
                return;
            }
            state.next_id += 1;
            let id = state.next_id;
            state.shown.replace(Shown { id, request, reply: Some(reply) })
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

    /// The user's answer to prompt `id`. The prompt goes off the screen.
    pub fn answer(&self, id: u64, reply: SignInReply) -> Result<(), String> {
        let shown = {
            let mut state = self.state();
            match &state.shown {
                Some(shown) if shown.id == id => state.shown.take(),
                _ => return Err(GONE.into()),
            }
        };
        if let Some(waiting) = shown.and_then(|shown| shown.reply) {
            let _ = waiting.send(reply);
        }
        self.changed();
        Ok(())
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
            state.shown.take()
        };
        if let Some(shown) = shown {
            shown.cancel();
            self.changed();
        }
    }
}
