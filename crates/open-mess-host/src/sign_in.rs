//! Sign-in imports (spec 8.3, 9.1). A module's sign-in prompts (a device code, a QR code, a sign-in
//! page) go to the daemon as [`SignInRequest`]s, which it shows in the configuration window; the
//! answer comes back on a oneshot. An import that waits for the user runs with the call deadline
//! paused, so the user's time never counts against the call limit; it has [`SIGN_IN_BUDGET`]
//! instead, and the user can cancel (spec 8.3, R4). Codes, URLs and tokens are never logged.

use std::future::Future;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::actor::Deadline;
use crate::bindings::open_mess::module::sign_in::{Host, SignInError};
use crate::runtime::HostState;

/// Test support: answers `wam-token` in place of Windows. It gets the scopes, whether the call is
/// interactive and the claims asked for, and returns a token or the reason it refuses
/// (`sign-in-error::refused`).
#[cfg(any(test, feature = "test-support"))]
pub type FakeWam = std::sync::Arc<dyn Fn(&[String], bool, Option<&str>) -> Result<String, String> + Send + Sync>;

/// The longest an import waits for the user's answer.
pub const SIGN_IN_BUDGET: Duration = Duration::from_secs(10 * 60);
/// The longest code, URL or QR text a module may show; a longer one is refused.
const MAX_PROMPT_TEXT: usize = 8 * 1024;
/// The answer when the daemon takes no prompts (no window, or the module is loading).
pub(crate) const UNAVAILABLE: &str = "sign-in UI not available";

/// What a module asks the user during sign-in, for the configuration window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInRequest {
    /// A device code to enter at `url`; nothing waits for an answer.
    DeviceCode { module: String, code: String, url: String },
    /// A QR code to scan; nothing waits for an answer.
    Qr { module: String, data: String },
    /// A sign-in page for the sign-in window; the answer is the URL of the first navigation that
    /// starts with `redirect_prefix`.
    OpenPage { module: String, url: String, redirect_prefix: String },
}

impl SignInRequest {
    /// The module that asks.
    pub fn module(&self) -> &str {
        match self {
            SignInRequest::DeviceCode { module, .. } | SignInRequest::Qr { module, .. } | SignInRequest::OpenPage { module, .. } => module,
        }
    }
}

/// The window's answer to a [`SignInRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInReply {
    /// Done; for a sign-in page, the URL it redirected to.
    Done(Option<String>),
    Cancelled,
    Refused(String),
}

/// A request and where its answer goes.
pub type Prompt = (SignInRequest, oneshot::Sender<SignInReply>);

/// Where a module's sign-in prompts go: the daemon's side of the configuration window.
#[derive(Clone)]
pub struct SignInPrompts {
    sender: mpsc::Sender<Prompt>,
    budget: Duration,
}

impl SignInPrompts {
    pub fn new(sender: mpsc::Sender<Prompt>) -> SignInPrompts {
        SignInPrompts { sender, budget: SIGN_IN_BUDGET }
    }

    /// Prompts with a shorter budget, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_budget(sender: mpsc::Sender<Prompt>, budget: Duration) -> SignInPrompts {
        SignInPrompts { sender, budget }
    }

    /// Shows a prompt that needs no answer.
    fn show(&self, request: SignInRequest) {
        let (reply, _) = oneshot::channel();
        if self.sender.try_send((request, reply)).is_err() {
            tracing::debug!("sign-in prompt not shown: {UNAVAILABLE}");
        }
    }

    /// Asks the user and waits for the answer, at most the budget.
    async fn ask(&self, request: SignInRequest) -> Result<SignInReply, SignInError> {
        let (reply, answer) = oneshot::channel();
        self.sender.send((request, reply)).await.map_err(|_| SignInError::Refused(UNAVAILABLE.to_string()))?;
        match tokio::time::timeout(self.budget, answer).await {
            Err(_) => Err(SignInError::TimedOut),
            // The daemon dropped the request: it stopped, or the window went away.
            Ok(Err(_)) => Err(SignInError::Cancelled),
            Ok(Ok(reply)) => Ok(reply),
        }
    }
}

/// Runs a sign-in step that waits for the user, with the call deadline paused meanwhile.
pub(crate) async fn wait_for_user<T>(deadline: &Deadline, step: impl Future<Output = T>) -> T {
    let _paused = deadline.pause();
    step.await
}

fn too_long(texts: &[&str]) -> bool {
    texts.iter().any(|text| text.len() > MAX_PROMPT_TEXT)
}

impl HostState {
    /// The module's prompts; none while the manifest is read at load (the module has no name).
    fn prompts(&self) -> Result<&SignInPrompts, SignInError> {
        if self.env.name.is_empty() {
            return Err(SignInError::Refused(UNAVAILABLE.to_string()));
        }
        Ok(&self.env.sign_in)
    }
}

impl Host for HostState {
    async fn show_device_code(&mut self, code: String, verification_url: String) {
        let Ok(prompts) = self.prompts() else { return };
        if too_long(&[&code, &verification_url]) {
            tracing::warn!(module = %self.env.name, "device code not shown: over {MAX_PROMPT_TEXT} bytes");
            return;
        }
        prompts.show(SignInRequest::DeviceCode { module: self.env.name.clone(), code, url: verification_url });
    }

    async fn show_qr(&mut self, data: String) {
        let Ok(prompts) = self.prompts() else { return };
        if too_long(&[&data]) {
            tracing::warn!(module = %self.env.name, "QR code not shown: over {MAX_PROMPT_TEXT} bytes");
            return;
        }
        prompts.show(SignInRequest::Qr { module: self.env.name.clone(), data });
    }

    async fn open_page(&mut self, url: String, redirect_prefix: String) -> Result<String, SignInError> {
        if too_long(&[&url, &redirect_prefix]) {
            return Err(SignInError::Refused(format!("the URL is over {MAX_PROMPT_TEXT} bytes")));
        }
        let request = SignInRequest::OpenPage { module: self.env.name.clone(), url, redirect_prefix };
        let prompts = self.prompts()?;
        match wait_for_user(&self.deadline, prompts.ask(request)).await? {
            SignInReply::Done(Some(url)) => Ok(url),
            SignInReply::Done(None) | SignInReply::Cancelled => Err(SignInError::Cancelled),
            SignInReply::Refused(why) => Err(SignInError::Refused(why)),
        }
    }

    async fn wam_token(&mut self, _scopes: Vec<String>, _interactive: bool, _claims: Option<String>) -> Result<String, SignInError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(wam) = &self.env.fake_wam {
            return wam(&_scopes, _interactive, _claims.as_deref()).map_err(SignInError::Refused);
        }
        Err(SignInError::Refused("Windows account sign-in not available yet".to_string()))
    }
}
