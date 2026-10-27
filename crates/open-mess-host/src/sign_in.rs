//! Sign-in imports (spec 8.3, 9.1). A module's sign-in prompts (a device code, a QR code, a sign-in
//! page, the choice of a Windows account) go to the daemon as [`SignInRequest`]s, which it shows in
//! the configuration window; the answer comes back on a oneshot. An import that waits for the user
//! runs with the call deadline paused, so the user's time never counts against the call limit; it
//! has [`SIGN_IN_BUDGET`] instead, and the user can cancel (spec 8.3, R4). Codes, URLs, account
//! names and tokens are never logged.
//!
//! `wam-token` gets tokens from Windows (wam.rs) for the identity the module's approved manifest
//! declares, never another client id or scope. In the module's `sign-in` the user picks the
//! Windows account in the window (or in the WAM dialog); once the sign-in succeeds, the account's
//! WAM id is kept as the module account's secret [`WAM_ACCOUNT`], and every later call for that
//! account gets its tokens silently with it.

use std::future::Future;
use std::time::{Duration, Instant};

use keyring_core::api::CredentialStore;
use tokio::sync::{mpsc, oneshot};

use crate::actor::Deadline;
use crate::bindings::open_mess::module::sign_in::{Host, SignInError};
use crate::bindings::open_mess::module::types;
use crate::runtime::{HostState, WamIdentity};
use crate::storage::Storage;
use crate::wam::{self, WamAccount};

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
/// The module secret that keeps the WAM id of a module account's Windows account.
pub const WAM_ACCOUNT: &str = "wam-account";
/// Why `wam-token` refuses the WAM dialog outside a sign-in from the window.
const DIALOG_ONLY_IN_SIGN_IN: &str = "the Windows sign-in dialog opens only when the user signs in from the configuration window";

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
    /// The Windows account to sign in with: `Done(Some(id))` picks one of `accounts`,
    /// `Done(None)` asks for the WAM dialog instead (another account).
    WamPick { module: String, accounts: Vec<WamAccount> },
    /// The window that owns the WAM dialog, answered with [`SignInReply::Window`] by the daemon
    /// itself, not the user.
    WamWindow { module: String },
}

impl SignInRequest {
    /// The module that asks.
    pub fn module(&self) -> &str {
        match self {
            SignInRequest::DeviceCode { module, .. }
            | SignInRequest::Qr { module, .. }
            | SignInRequest::OpenPage { module, .. }
            | SignInRequest::WamPick { module, .. }
            | SignInRequest::WamWindow { module } => module,
        }
    }
}

/// The window's answer to a [`SignInRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInReply {
    /// Done; for a sign-in page, the URL it redirected to; for a WAM pick, the account's id.
    Done(Option<String>),
    Cancelled,
    Refused(String),
    /// The configuration window's handle (`HWND`), for [`SignInRequest::WamWindow`].
    Window(isize),
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

    /// Shows a prompt the user only acts on (a code): the answer comes only when they cancel it.
    fn show(&self, request: SignInRequest) -> Option<oneshot::Receiver<SignInReply>> {
        let (reply, answer) = oneshot::channel();
        match self.sender.try_send((request, reply)) {
            Ok(()) => Some(answer),
            Err(_) => {
                tracing::debug!("sign-in prompt not shown: {UNAVAILABLE}");
                None
            }
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

/// Device or QR codes shown in a `sign-in` call, which the module polls for (with `http.fetch`)
/// while the user uses them elsewhere: the call's clock stops meanwhile, up to one budget for all
/// of them. Kept for the rest of the call.
pub(crate) struct CodeShown {
    /// The answer to the code on screen, answered (or dropped) once the user cancels or the
    /// sign-in ends; None while the clock runs.
    answer: Option<oneshot::Receiver<SignInReply>>,
    /// When the budget runs out.
    until: Instant,
}

/// After a code's budget, a module that still polls (fetch is refused by then) has this long
/// before the call is stopped.
const CODE_GRACE: Duration = Duration::from_secs(60);

/// Runs a sign-in step that waits for the user, with the call deadline paused meanwhile.
pub(crate) async fn wait_for_user<T>(deadline: &Deadline, step: impl Future<Output = T>) -> T {
    let _paused = deadline.pause();
    step.await
}

fn too_long(texts: &[&str]) -> bool {
    texts.iter().any(|text| text.len() > MAX_PROMPT_TEXT)
}

/// Whose call runs, which decides the Windows account of `wam-token`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Caller {
    /// An event (a timer, a socket frame) or the manifest: no account named.
    #[default]
    Event,
    /// The module's `sign-in` export.
    SignIn,
    /// A call for one of the module's accounts.
    Account(String),
}

/// What the WAM dialog gives: a token and the account the user chose.
type DialogResult = Result<(String, WamAccount), SignInError>;

/// The Windows token calls of wam.rs; tests replace them.
#[derive(Clone, Copy)]
pub(crate) struct WamCalls {
    pub(crate) accounts: fn(&str) -> Result<Vec<WamAccount>, SignInError>,
    pub(crate) silent: fn(&str, &[String], &str) -> Result<String, SignInError>,
    pub(crate) interactive: fn(isize, &str, &[String], Duration) -> DialogResult,
}

const WINDOWS: WamCalls = WamCalls { accounts: wam::accounts, silent: wam::token_silent, interactive: wam::token_interactive };

/// Runs a blocking WAM call off the async threads.
async fn blocking<T: Send + 'static>(call: impl FnOnce() -> Result<T, SignInError> + Send + 'static) -> Result<T, SignInError> {
    tokio::task::spawn_blocking(call).await.unwrap_or_else(|e| Err(SignInError::Refused(format!("Windows account call failed: {e}"))))
}

/// The manifest's WAM identity, if it covers every scope asked for: a module gets tokens only for
/// the client id and scopes the user approved (spec 8.3, R3).
fn allowed<'a>(identity: Option<&'a WamIdentity>, scopes: &[String]) -> Result<&'a WamIdentity, SignInError> {
    let identity = identity.ok_or_else(|| SignInError::Refused("the module's manifest declares no Windows account identity".to_string()))?;
    if scopes.is_empty() {
        return Err(SignInError::Refused("no scope asked for".to_string()));
    }
    match scopes.iter().find(|scope| !identity.scopes.contains(scope)) {
        Some(scope) => Err(SignInError::Refused(format!("scope {scope:?} is not in the module's manifest"))),
        None => Ok(identity),
    }
}

/// What `wam-token` works with, besides the call's own state.
pub(crate) struct Wam<'a> {
    pub(crate) module: &'a str,
    pub(crate) identity: Option<&'a WamIdentity>,
    pub(crate) prompts: &'a SignInPrompts,
    pub(crate) deadline: &'a Deadline,
    pub(crate) store: &'a CredentialStore,
    pub(crate) calls: WamCalls,
}

impl Wam<'_> {
    /// A token for `scopes`. In a sign-in the user picks the account (once; `pick` keeps it for
    /// the rest of the call), or the WAM dialog runs when `interactive`; any other call uses the
    /// account's stored one, silently. `windows_accounts` are the module accounts with a stored
    /// Windows account, which an event uses.
    async fn token(
        &self,
        windows_accounts: &[String],
        caller: &Caller,
        pick: &mut Option<WamAccount>,
        scopes: &[String],
        interactive: bool,
    ) -> Result<String, SignInError> {
        let client = allowed(self.identity, scopes)?.client_id.clone();
        let account = match caller {
            Caller::SignIn => match pick {
                Some(account) if !interactive => account.id.clone(),
                _ => {
                    let (account, token) = self.choose(&client, scopes, interactive).await?;
                    let id = account.id.clone();
                    *pick = Some(account);
                    if let Some(token) = token {
                        return Ok(token);
                    }
                    id
                }
            },
            _ if interactive => return Err(SignInError::Refused(DIALOG_ONLY_IN_SIGN_IN.to_string())),
            Caller::Account(account) => self.stored(account)?,
            // An event names no account: the module's only Windows account.
            Caller::Event => match windows_accounts {
                [account] => self.stored(account)?,
                [] => return Err(SignInError::Refused("no account of this module signed in with Windows".to_string())),
                _ => return Err(SignInError::Refused("several Windows accounts: ask for a token in a call for one of them".to_string())),
            },
        };
        let (silent, scopes) = (self.calls.silent, scopes.to_vec());
        blocking(move || silent(&client, &scopes, &account)).await
    }

    /// The WAM id stored for a module account.
    fn stored(&self, account: &str) -> Result<String, SignInError> {
        crate::secrets::get(self.store, self.module, account, WAM_ACCOUNT)
            .ok_or_else(|| SignInError::Refused("this account has no Windows account; sign in again".to_string()))
    }

    /// The Windows account of a sign-in: one the user picks in the window from those Windows knows
    /// for the client id, or through the WAM dialog, which gives a token too.
    async fn choose(&self, client: &str, scopes: &[String], interactive: bool) -> Result<(WamAccount, Option<String>), SignInError> {
        let module = self.module.to_string();
        if !interactive {
            let (list, id) = (self.calls.accounts, client.to_string());
            let accounts = blocking(move || list(&id)).await?;
            let request = SignInRequest::WamPick { module: module.clone(), accounts: accounts.clone() };
            match wait_for_user(self.deadline, self.prompts.ask(request)).await? {
                SignInReply::Done(Some(id)) => {
                    let picked = accounts.into_iter().find(|account| account.id == id);
                    return picked.map(|account| (account, None)).ok_or_else(|| SignInError::Refused("no such Windows account".to_string()));
                }
                // Another account: the WAM dialog.
                SignInReply::Done(None) => {}
                SignInReply::Refused(why) => return Err(SignInError::Refused(why)),
                SignInReply::Cancelled | SignInReply::Window(_) => return Err(SignInError::Cancelled),
            }
        }
        let window = match self.prompts.ask(SignInRequest::WamWindow { module }).await? {
            SignInReply::Window(window) => window,
            SignInReply::Refused(why) => return Err(SignInError::Refused(why)),
            _ => return Err(SignInError::Cancelled),
        };
        let (dialog, client, scopes) = (self.calls.interactive, client.to_string(), scopes.to_vec());
        // The dialog has the sign-in budget too: WAM cancels one left open past it, and the wait
        // here ends then too, whatever WAM does.
        let budget = self.prompts.budget;
        let waiting = tokio::time::timeout(budget, blocking(move || dialog(window, &client, &scopes, budget)));
        let (token, account) = wait_for_user(self.deadline, waiting).await.map_err(|_| SignInError::TimedOut)??;
        Ok((account, Some(token)))
    }
}

/// Keeps the Windows account the user picked in a sign-in as the secret of the module account the
/// sign-in returned.
fn remember(store: &CredentialStore, storage: &mut Storage, module: &str, account: &str, picked: &WamAccount) -> Result<(), String> {
    crate::secrets::set(store, storage, module, account, WAM_ACCOUNT, &picked.id)
}

impl HostState {
    /// The module's prompts; none while the manifest is read at load (the module has no name).
    fn prompts(&self) -> Result<&SignInPrompts, SignInError> {
        if self.env.name.is_empty() {
            return Err(SignInError::Refused(UNAVAILABLE.to_string()));
        }
        Ok(&self.env.sign_in)
    }

    /// Shows a device or QR code. In a sign-in the clock stops until the user cancels it, the
    /// budget runs out or the module moves on (spec 8.3, R4).
    fn show_code(&mut self, request: SignInRequest) {
        // The budget is the code wait's, not each code's: a module that shows code after code gets
        // no more time, and none at all once the user cancelled.
        let now = Instant::now();
        let abandoned = self.code_abandoned().is_some();
        self.code_done();
        let Ok(prompts) = self.prompts() else { return };
        let until = match &self.code_shown {
            _ if abandoned => now,
            Some(shown) => shown.until,
            None => now + prompts.budget,
        };
        let answer = prompts.show(request).filter(|_| self.caller == Caller::SignIn && until > now);
        if answer.is_some() {
            self.deadline.pause_until(until + CODE_GRACE);
        }
        self.code_shown = Some(CodeShown { answer, until });
    }

    /// The module moved on from a code on screen: the clock runs again.
    fn code_done(&mut self) {
        if let Some(shown) = &mut self.code_shown
            && shown.answer.take().is_some()
        {
            self.deadline.resume();
        }
    }

    /// Why the module should stop polling for the code on screen: the user cancelled it (or the
    /// sign-in ended), or its budget ran out. The clock runs again from then on.
    pub(crate) fn code_abandoned(&mut self) -> Option<&'static str> {
        let shown = self.code_shown.as_mut()?;
        let answer = shown.answer.as_mut()?;
        let why = match answer.try_recv() {
            Err(oneshot::error::TryRecvError::Empty) if Instant::now() < shown.until => return None,
            Err(oneshot::error::TryRecvError::Empty) => "sign-in timed out",
            _ => "sign-in cancelled",
        };
        self.code_done();
        Some(why)
    }

    /// Ends a `sign-in` call: the Windows account picked in it becomes the secret of the account
    /// it signed in. A sign-in whose pick cannot be kept fails, since later calls could not get
    /// tokens.
    pub(crate) fn signed_in(&mut self, result: Result<types::AccountInfo, types::ModuleError>) -> Result<types::AccountInfo, types::ModuleError> {
        let (Ok(info), Some(picked)) = (&result, self.pick.take()) else { return result };
        let keep = |state: &mut HostState| {
            let store = state.secret_store()?;
            let module = state.env.name.clone();
            let storage = state.storage().map_err(|e| format!("{e:?}"))?;
            remember(&*store, storage, &module, &info.id, &picked)
        };
        match keep(self) {
            Ok(()) => result,
            Err(why) => Err(types::ModuleError::Platform(format!("cannot keep the Windows account: {why}"))),
        }
    }
}

impl Host for HostState {
    async fn show_device_code(&mut self, code: String, verification_url: String) {
        if too_long(&[&code, &verification_url]) {
            tracing::warn!(module = %self.env.name, "device code not shown: over {MAX_PROMPT_TEXT} bytes");
            return;
        }
        self.show_code(SignInRequest::DeviceCode { module: self.env.name.clone(), code, url: verification_url });
    }

    async fn show_qr(&mut self, data: String) {
        if too_long(&[&data]) {
            tracing::warn!(module = %self.env.name, "QR code not shown: over {MAX_PROMPT_TEXT} bytes");
            return;
        }
        self.show_code(SignInRequest::Qr { module: self.env.name.clone(), data });
    }

    async fn open_page(&mut self, url: String, redirect_prefix: String) -> Result<String, SignInError> {
        self.code_done();
        if too_long(&[&url, &redirect_prefix]) {
            return Err(SignInError::Refused(format!("the URL is over {MAX_PROMPT_TEXT} bytes")));
        }
        let request = SignInRequest::OpenPage { module: self.env.name.clone(), url, redirect_prefix };
        let prompts = self.prompts()?;
        match wait_for_user(&self.deadline, prompts.ask(request)).await? {
            SignInReply::Done(Some(url)) => Ok(url),
            SignInReply::Refused(why) => Err(SignInError::Refused(why)),
            SignInReply::Done(None) | SignInReply::Cancelled | SignInReply::Window(_) => Err(SignInError::Cancelled),
        }
    }

    /// A token from Windows for the manifest's identity; `interactive` opens the WAM dialog,
    /// which only a sign-in from the window may do. A silent request that needs the dialog fails
    /// with `refused("interaction required")`.
    async fn wam_token(&mut self, scopes: Vec<String>, interactive: bool, _claims: Option<String>) -> Result<String, SignInError> {
        self.code_done();
        #[cfg(any(test, feature = "test-support"))]
        if let Some(wam) = &self.env.fake_wam {
            return wam(&scopes, interactive, _claims.as_deref()).map_err(SignInError::Refused);
        }
        if scopes.iter().map(String::len).sum::<usize>() > MAX_PROMPT_TEXT {
            return Err(SignInError::Refused(format!("the scopes are over {MAX_PROMPT_TEXT} bytes")));
        }
        self.prompts()?;
        // Refused before anything touches Windows or the credential store.
        allowed(self.env.wam.as_ref(), &scopes)?;
        let store = self.secret_store().map_err(SignInError::Refused)?;
        let windows_accounts = match self.caller {
            Caller::Event => {
                self.storage().and_then(|storage| storage.secret_accounts(WAM_ACCOUNT)).map_err(|e| SignInError::Refused(format!("{e:?}")))?
            }
            _ => Vec::new(),
        };
        let (env, deadline, caller, mut pick) = (std::sync::Arc::clone(&self.env), self.deadline.clone(), self.caller.clone(), self.pick.take());
        let wam = Wam { module: &env.name, identity: env.wam.as_ref(), prompts: &env.sign_in, deadline: &deadline, store: &*store, calls: WINDOWS };
        let token = wam.token(&windows_accounts, &caller, &mut pick, &scopes, interactive).await;
        self.pick = pick;
        token
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const CLIENT: &str = "client-x";
    const SCOPE: &str = "https://chat.example.com/.default";

    fn identity() -> WamIdentity {
        WamIdentity { client_id: CLIENT.into(), scopes: vec![SCOPE.into()] }
    }

    fn scopes() -> Vec<String> {
        vec![SCOPE.into()]
    }

    fn account(id: &str) -> WamAccount {
        WamAccount { id: id.into(), user_name: format!("{id}@contoso.example"), tenant: "t1".into() }
    }

    /// Windows with accounts w1 and w2; a token names its client id, scopes and account.
    const FAKE: WamCalls = WamCalls {
        accounts: |_| Ok(vec![account("w1"), account("w2")]),
        silent: |client, scopes, account| Ok(format!("token {client} {} {account}", scopes.join(" "))),
        interactive: |window, client, _, _| Ok((format!("dialog {window} {client}"), account("w3"))),
    };

    struct Setup {
        _dir: tempfile::TempDir,
        store: Arc<CredentialStore>,
        storage: Storage,
        prompts: SignInPrompts,
        asked: mpsc::Receiver<Prompt>,
        deadline: Deadline,
        identity: WamIdentity,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path(), "m").unwrap();
        let (sender, asked) = mpsc::channel(8);
        let prompts = SignInPrompts::with_budget(sender, Duration::from_secs(5));
        Setup {
            _dir: dir,
            store: keyring_core::mock::Store::new().unwrap(),
            storage,
            prompts,
            asked,
            deadline: Deadline::default(),
            identity: identity(),
        }
    }

    impl Setup {
        fn wam(&self, calls: WamCalls) -> Wam<'_> {
            Wam { module: "m", identity: Some(&self.identity), prompts: &self.prompts, deadline: &self.deadline, store: &*self.store, calls }
        }

        async fn token(&self, caller: &Caller, pick: &mut Option<WamAccount>, interactive: bool) -> Result<String, SignInError> {
            let windows_accounts = self.storage.secret_accounts(WAM_ACCOUNT).unwrap();
            self.wam(FAKE).token(&windows_accounts, caller, pick, &scopes(), interactive).await
        }
    }

    fn refused(result: Result<String, SignInError>) -> String {
        match result {
            Err(SignInError::Refused(why)) => why,
            other => panic!("not refused: {other:?}"),
        }
    }

    #[tokio::test]
    async fn wam_is_limited_to_the_manifest() {
        let mut t = setup();
        let no_wam = WamCalls { accounts: |_| panic!("listed"), silent: |_, _, _| panic!("asked"), interactive: |_, _, _, _| panic!("dialog") };
        let mut pick = Some(account("w1"));
        // A scope the user did not approve never reaches Windows, nor does a module without a
        // WAM identity.
        for scopes in [vec!["https://graph.example.com/.default".to_string()], vec![SCOPE.into(), "openid".into()], Vec::new()] {
            assert!(refused(t.wam(no_wam).token(&[], &Caller::SignIn, &mut pick, &scopes, false).await).contains("scope"));
        }
        t.identity.scopes.clear();
        assert!(t.wam(no_wam).token(&[], &Caller::SignIn, &mut pick, &scopes(), false).await.is_err());
        let mut wam = t.wam(no_wam);
        wam.identity = None;
        assert!(refused(wam.token(&[], &Caller::SignIn, &mut pick, &scopes(), false).await).contains("no Windows account identity"));
        // The client id is always the manifest's: the module has no way to name another.
        let t = setup();
        assert_eq!(t.token(&Caller::SignIn, &mut pick, false).await.unwrap(), format!("token {CLIENT} {SCOPE} w1"));
    }

    #[tokio::test]
    async fn stored_account_is_reused() {
        let mut t = setup();
        remember(&*t.store, &mut t.storage, "m", "acct", &account("w2")).unwrap();
        // A call for the account and an event both get its token silently, without asking.
        assert_eq!(t.token(&Caller::Account("acct".into()), &mut None, false).await.unwrap(), format!("token {CLIENT} {SCOPE} w2"));
        assert_eq!(t.token(&Caller::Event, &mut None, false).await.unwrap(), format!("token {CLIENT} {SCOPE} w2"));
        assert!(t.asked.try_recv().is_err());
        // An account that never signed in with Windows, or the dialog outside a sign-in: refused.
        assert!(refused(t.token(&Caller::Account("other".into()), &mut None, false).await).contains("sign in again"));
        assert_eq!(refused(t.token(&Caller::Account("acct".into()), &mut None, true).await), DIALOG_ONLY_IN_SIGN_IN);
        // With two Windows accounts, an event can't tell which one.
        remember(&*t.store, &mut t.storage, "m", "acct2", &account("w1")).unwrap();
        assert!(refused(t.token(&Caller::Event, &mut None, false).await).contains("several"));
    }

    #[tokio::test]
    async fn pick_is_stored_as_module_secret() {
        let mut t = setup();
        let mut asked = std::mem::replace(&mut t.asked, mpsc::channel(1).1);
        let user = tokio::spawn(async move {
            // The window lists what Windows knows; the user picks w2.
            let (request, reply) = asked.recv().await.unwrap();
            assert_eq!(request, SignInRequest::WamPick { module: "m".into(), accounts: vec![account("w1"), account("w2")] });
            reply.send(SignInReply::Done(Some("w2".into()))).unwrap();
            // Then another sign-in, with another account: the WAM dialog over the window.
            let (request, reply) = asked.recv().await.unwrap();
            assert_eq!(request, SignInRequest::WamPick { module: "m".into(), accounts: vec![account("w1"), account("w2")] });
            reply.send(SignInReply::Done(None)).unwrap();
            let (request, reply) = asked.recv().await.unwrap();
            assert_eq!(request, SignInRequest::WamWindow { module: "m".into() });
            reply.send(SignInReply::Window(42)).unwrap();
            asked
        });
        let mut pick = None;
        assert_eq!(t.token(&Caller::SignIn, &mut pick, false).await.unwrap(), format!("token {CLIENT} {SCOPE} w2"));
        // The rest of the sign-in uses the pick without asking again.
        assert_eq!(t.token(&Caller::SignIn, &mut pick, false).await.unwrap(), format!("token {CLIENT} {SCOPE} w2"));
        assert_eq!(pick, Some(account("w2")));
        remember(&*t.store, &mut t.storage, "m", "acct", &account("w2")).unwrap();
        assert_eq!(crate::secrets::get(&*t.store, "m", "acct", WAM_ACCOUNT).as_deref(), Some("w2"));

        let mut pick = None;
        assert_eq!(t.token(&Caller::SignIn, &mut pick, false).await.unwrap(), format!("dialog 42 {CLIENT}"));
        assert_eq!(pick, Some(account("w3")));
        let mut asked = user.await.unwrap();
        assert!(asked.try_recv().is_err());
    }

    #[tokio::test]
    async fn dialog_has_the_budget() {
        let mut t = setup();
        let (sender, mut asked) = mpsc::channel::<Prompt>(8);
        t.prompts = SignInPrompts::with_budget(sender, Duration::from_millis(200));
        tokio::spawn(async move {
            while let Some((request, reply)) = asked.recv().await {
                let _ = reply.send(if matches!(request, SignInRequest::WamWindow { .. }) { SignInReply::Window(1) } else { SignInReply::Done(None) });
            }
        });
        // The user leaves the WAM dialog open.
        let slow = WamCalls {
            interactive: |_, _, _, _| {
                std::thread::sleep(Duration::from_secs(1));
                Ok(("late".into(), account("w9")))
            },
            ..FAKE
        };
        let mut pick = None;
        let result = t.wam(slow).token(&[], &Caller::SignIn, &mut pick, &scopes(), true).await;
        assert!(matches!(result, Err(SignInError::TimedOut)), "{result:?}");
        assert_eq!(pick, None);
    }

    #[tokio::test]
    async fn closing_the_picker_cancels() {
        let mut t = setup();
        let mut asked = std::mem::replace(&mut t.asked, mpsc::channel(1).1);
        tokio::spawn(async move {
            let (_, reply) = asked.recv().await.unwrap();
            reply.send(SignInReply::Cancelled).unwrap();
        });
        let mut pick = None;
        assert!(matches!(t.token(&Caller::SignIn, &mut pick, false).await, Err(SignInError::Cancelled)));
        assert_eq!(pick, None);
    }
}
