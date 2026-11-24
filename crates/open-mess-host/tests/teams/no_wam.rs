//! The Teams module on a host without WAM (spec 2026-11-20-teams-sign-in-without-wam, 7.3-7.8):
//! sign-in through the sign-in page (authorization code with PKCE) that the test answers itself,
//! and every later token from the refresh token in the account's secrets. The fake server plays
//! the token endpoint of `login.microsoftonline.com`, with synthetic tokens.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{
    AccountInfo, Engine, HostError, HostEvent, ModuleContext, ModuleError, ModuleHandle, Net, Prompt, SessionState, SignInPrompts, SignInReply,
    SignInRequest, limits,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use url::{Url, form_urlencoded};

use crate::fake::{self, CHAT_PATH, Fake, ME_NAME, Reply, Seen};
use crate::{AUTHSVC_SCOPE, CHAT_SCOPE, GROUP, common, conversations_page, module_error};

const TOKEN_PATH: &str = "/organizations/oauth2/v2.0/token";
const NATIVECLIENT: &str = "https://login.microsoftonline.com/common/oauth2/nativeclient";
const CAE_CLAIMS: &str = r#"{"access_token":{"xms_cc":{"values":["CP1"]}}}"#;
/// The code the test's sign-in page hands back.
const CODE: &str = "synthetic-code";

/// The module's secrets live in keyring-core's default store, which a container does not have in
/// the form the daemon uses: one in-memory store for the whole test process. The tests do not
/// share an account, so they do not share secrets. The tests with WAM run in the same process, but
/// with WAM the module never reaches the secrets (`with_wam_the_secrets_are_left_alone`), so none
/// of them sets up Windows' own store meanwhile.
fn mock_secrets() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap()));
}

/// An account id of its own for each test.
fn account() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(100);
    format!("00000000-0000-0000-0000-{:012}", NEXT.fetch_add(1, Ordering::SeqCst))
}

/// The token endpoint of the fake: unsigned tokens for the account `oid` and the audience of the
/// asked scope, and a refresh token that changes with every answer (`rt-1`, `rt-2`, ...).
#[derive(Clone)]
struct Idp {
    oid: String,
    issued: Arc<AtomicUsize>,
    /// Refresh requests get `invalid_grant` while this is set.
    revoked: Arc<AtomicBool>,
}

impl Idp {
    fn answer(&self, request: &Seen) -> Option<Reply> {
        if request.path != TOKEN_PATH {
            return None;
        }
        let form = form(request);
        if form.get("grant_type").map(String::as_str) == Some("refresh_token") && self.revoked.load(Ordering::SeqCst) {
            let error = json!({
                "error": "invalid_grant",
                "error_description": "AADSTS70008: The refresh token has expired for person.one@synthetic.example.",
                "error_codes": [70008],
            });
            return Some(Reply::json(error).with_status(400));
        }
        let scope = form.get("scope").cloned().unwrap_or_default();
        let audience = scope.split(' ').next().unwrap_or_default().trim_end_matches("/.default");
        let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
        let access = fake::jwt(&json!({
            "aud": audience,
            "oid": self.oid,
            "tid": "00000000-0000-0000-0000-0000000000aa",
            "name": ME_NAME,
            "exp": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600,
            "n": n,
        }));
        Some(Reply::json(json!({"access_token": access, "refresh_token": format!("rt-{n}"), "expires_in": 3600, "token_type": "Bearer"})))
    }
}

/// The fields of a form-urlencoded request body.
fn form(request: &Seen) -> HashMap<String, String> {
    form_urlencoded::parse(&request.body).into_owned().collect()
}

/// The module on a host without WAM: its sign-in page prompts go to `prompts`, which the test
/// answers.
struct NoWam {
    module: ModuleHandle,
    events: mpsc::Receiver<HostEvent>,
    prompts: mpsc::Receiver<Prompt>,
    fake: Fake,
    idp: Idp,
    oid: String,
    data: tempfile::TempDir,
}

async fn load(fake: &Fake, data_dir: &Path) -> (ModuleHandle, mpsc::Receiver<HostEvent>, mpsc::Receiver<Prompt>) {
    mock_secrets();
    let engine = Engine::with_limits(limits::for_tests()).unwrap().without_wam();
    let net = Net::with_limits(AddressPolicy::AllowLoopbackForTests, limits::for_tests()).serving_hosts_from(fake.addr);
    let (events, event_receiver) = mpsc::channel(64);
    let (sender, prompts) = mpsc::channel(8);
    let ctx = ModuleContext { data_dir: data_dir.to_path_buf(), events, net, sign_in: SignInPrompts::new(sender) };
    let module = engine.load(&common::wasm("open_mess_teams"), ctx).await.unwrap();
    (module, event_receiver, prompts)
}

impl NoWam {
    async fn start() -> NoWam {
        let (data, fake, oid) = (tempfile::tempdir().unwrap(), Fake::start().await, account());
        let (module, events, prompts) = load(&fake, data.path()).await;
        let idp = Idp { oid: oid.clone(), issued: Arc::default(), revoked: Arc::default() };
        let teams = NoWam { module, events, prompts, fake, idp, oid, data };
        teams.serve(|_| None);
        teams
    }

    async fn signed_in() -> NoWam {
        let mut teams = NoWam::start().await;
        teams.sign_in().await.unwrap();
        teams
    }

    /// The token endpoint, and `other` for everything else (the conversation list by default).
    fn serve(&self, other: impl Fn(&Seen) -> Option<Reply> + Send + Sync + 'static) {
        let (idp, chat) = (self.idp.clone(), conversations_page(Vec::new()));
        self.fake.on(move |s| idp.answer(s).or_else(|| other(s)).or_else(|| chat(s)));
    }

    /// Signs in; the page is answered with this sign-in's own `state` and the synthetic code.
    async fn sign_in(&mut self) -> Result<AccountInfo, HostError> {
        let (result, _) = tokio::join!(self.module.sign_in(), answer_page(&mut self.prompts, None));
        result
    }

    /// The module loaded again on the same data folder, as after a daemon restart.
    async fn reload(&mut self) {
        (self.module, self.events, self.prompts) = load(&self.fake, self.data.path()).await;
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, HostError> {
        let result = self.module.call(&self.oid, tool, &args.to_string()).await?;
        Ok(serde_json::from_str(&result).unwrap())
    }

    /// The forms the token endpoint got, in order.
    fn token_requests(&self) -> Vec<HashMap<String, String>> {
        self.fake.requests(TOKEN_PATH).iter().map(form).collect()
    }

    /// The refresh token in the account's secrets, read the way the host stores it.
    fn stored_refresh_token(&self) -> Option<String> {
        let store = keyring_core::get_default_store().unwrap();
        store.build("open_mess/module/teams", &format!("{}/refresh-token", self.oid), None).unwrap().get_password().ok()
    }
}

/// Takes the sign-in page prompt and answers it as the redirect would: with `state` (the page's
/// own when `None`) and the synthetic code. Returns the page's query.
async fn answer_page(prompts: &mut mpsc::Receiver<Prompt>, state: Option<&str>) -> HashMap<String, String> {
    let (request, reply) = tokio::time::timeout(Duration::from_secs(5), prompts.recv()).await.unwrap().unwrap();
    let SignInRequest::OpenPage { module, url, redirect_prefix } = request else { panic!("expected a sign-in page, got {request:?}") };
    assert_eq!(module, "teams");
    assert_eq!(redirect_prefix, NATIVECLIENT);
    let url = Url::parse(&url).unwrap();
    assert_eq!((url.host_str(), url.path()), (Some("login.microsoftonline.com"), "/organizations/oauth2/v2.0/authorize"));
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let state = state.unwrap_or(&query["state"]);
    let answer = format!(
        "{redirect_prefix}?{}",
        form_urlencoded::Serializer::new(String::new()).append_pair("code", CODE).append_pair("state", state).finish()
    );
    reply.send(SignInReply::Done(Some(answer))).unwrap();
    query
}

#[tokio::test]
async fn sign_in_without_wam_exchanges_the_code_and_keeps_the_refresh_token() {
    let mut teams = NoWam::start().await;
    let (result, page) = tokio::join!(teams.module.sign_in(), answer_page(&mut teams.prompts, None));
    assert_eq!(result.unwrap(), AccountInfo { id: teams.oid.clone(), display_name: ME_NAME.to_string() });

    // One exchange, for this sign-in: the verifier hashes to the challenge the page was sent.
    let requests = teams.fake.requests(TOKEN_PATH);
    assert_eq!(requests.len(), 1);
    let (request, form) = (&requests[0], form(&requests[0]));
    assert_eq!((request.method.as_str(), request.host.as_str()), ("POST", "login.microsoftonline.com"));
    assert_eq!(request.header("content-type"), Some("application/x-www-form-urlencoded"));
    assert_eq!(request.header("origin"), None);
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], CODE);
    assert_eq!(form["redirect_uri"], NATIVECLIENT);
    assert_eq!(page["code_challenge_method"], "S256");
    assert_eq!(fake::base64url(&Sha256::digest(form["code_verifier"].as_bytes())), page["code_challenge"]);
    assert_eq!(form["client_id"], page["client_id"]);
    assert_eq!(form["claims"], CAE_CLAIMS);

    // Teams itself accepted the account, with the token of the exchange.
    let authz = teams.fake.requests(fake::AUTHZ_PATH);
    assert_eq!(authz.len(), 1);
    assert_eq!(authz[0].audience().as_deref(), Some("https://api.spaces.skype.com"));
    assert_eq!(teams.stored_refresh_token().as_deref(), Some("rt-1"));
}

#[tokio::test]
async fn restore_without_wam_refreshes_from_the_stored_token_and_sends_no_prompt() {
    let mut teams = NoWam::signed_in().await;
    let before = teams.token_requests().len();
    teams.reload().await;
    teams.module.restore(&teams.oid).await.unwrap();

    let requests = teams.token_requests();
    assert_eq!(requests.len(), before + 1);
    let refresh = &requests[before];
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "rt-1");
    assert_eq!(refresh["scope"], format!("{AUTHSVC_SCOPE} offline_access"));
    assert_eq!(teams.fake.requests(fake::AUTHZ_PATH).len(), 2);
    assert!(teams.prompts.try_recv().is_err(), "the user is not asked again");
    assert_eq!(teams.stored_refresh_token().as_deref(), Some("rt-2"));
}

#[tokio::test]
async fn restore_without_wam_and_without_a_stored_token_is_auth_required() {
    let mut teams = NoWam::start().await;
    assert!(matches!(module_error(teams.module.restore(&teams.oid).await), ModuleError::AuthRequired(_)));
    assert!(teams.fake.seen().is_empty());
    assert!(teams.prompts.try_recv().is_err());
}

#[tokio::test]
async fn chat_token_comes_from_the_refresh_token_and_the_new_one_replaces_it() {
    let teams = NoWam::signed_in().await;
    teams.call("list_conversations", json!({"limit": 50})).await.unwrap();

    let requests = teams.token_requests();
    assert_eq!(requests.len(), 2);
    let refresh = &requests[1];
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "rt-1");
    assert_eq!(refresh["scope"], format!("{CHAT_SCOPE} offline_access"));
    assert_eq!(refresh["claims"], CAE_CLAIMS);
    let chat = teams.fake.requests(CHAT_PATH);
    assert_eq!(chat.len(), 1);
    assert_eq!(chat[0].audience().as_deref(), Some("https://ic3.teams.office.com"));
    // The answer's refresh token is kept, and the next token is asked with it.
    assert_eq!(teams.stored_refresh_token().as_deref(), Some("rt-2"));
    teams.call("list_conversations", json!({})).await.unwrap();
    assert_eq!(teams.token_requests().len(), 2, "the chat token is kept until it expires");
}

#[tokio::test]
async fn cae_challenge_on_chat_refreshes_with_the_challenge_and_cp1() {
    let teams = NoWam::signed_in().await;
    let claims = r#"{"access_token":{"nbf":{"essential":true,"value":"1800000000"}}}"#;
    let header = format!(r#"Bearer realm="", error="insufficient_claims", claims="{}""#, fake::base64url(claims.as_bytes()));
    let refused = Arc::new(AtomicUsize::new(1));
    let page = conversations_page(vec![fake::conversation(GROUP, Some("Synthetic group"), None)]);
    teams.serve(move |s| {
        let reply = page(s)?;
        Some(if refused.fetch_sub(1, Ordering::SeqCst) > 0 { Reply::status(401).header("WWW-Authenticate", &header) } else { reply })
    });
    let result = teams.call("list_conversations", json!({})).await.unwrap();
    assert_eq!(result["items"].as_array().unwrap().len(), 1);

    // Sign-in, the chat token, and the chat token again with the challenge.
    let requests = teams.token_requests();
    assert_eq!(requests.len(), 3);
    let sent: Value = serde_json::from_str(&requests[2]["claims"]).unwrap();
    assert_eq!(sent["access_token"]["nbf"], json!({"essential": true, "value": "1800000000"}));
    assert_eq!(sent["access_token"]["xms_cc"], json!({"values": ["CP1"]}));
    assert_eq!(requests[2]["scope"], format!("{CHAT_SCOPE} offline_access"));
    // The call is retried with the new token.
    let chat = teams.fake.requests(CHAT_PATH);
    assert_eq!(chat.len(), 2);
    assert_ne!(chat[0].header("authorization"), chat[1].header("authorization"));
}

#[tokio::test]
async fn invalid_grant_on_refresh_is_auth_required() {
    let mut teams = NoWam::signed_in().await;
    teams.idp.revoked.store(true, Ordering::SeqCst);
    let error = module_error(teams.call("list_conversations", json!({})).await);
    // Only the error and its AADSTS number: the description can name the user.
    assert_eq!(error, ModuleError::AuthRequired("Microsoft asks to sign in again (AADSTS70008)".to_string()));
    let event = tokio::time::timeout(Duration::from_secs(1), teams.events.recv()).await.unwrap().unwrap();
    assert_eq!(event, HostEvent::Session { module: "teams".to_string(), account: teams.oid.clone(), state: SessionState::AuthRequired });
    // The secret stays: signing in again overwrites it.
    assert_eq!(teams.stored_refresh_token().as_deref(), Some("rt-1"));
    assert!(teams.fake.requests(CHAT_PATH).is_empty());
}

#[tokio::test]
async fn sign_in_page_answer_for_another_state_is_refused_without_a_token_request() {
    let mut teams = NoWam::start().await;
    let (result, _) = tokio::join!(teams.module.sign_in(), answer_page(&mut teams.prompts, Some("another-state")));
    let error = module_error(result);
    assert_eq!(error, ModuleError::AuthRequired("the sign-in answer is not for this sign-in".to_string()));
    assert!(teams.fake.seen().is_empty(), "the code of another sign-in is never redeemed");
    assert_eq!(teams.stored_refresh_token(), None);
}

#[tokio::test]
async fn with_wam_the_secrets_are_left_alone() {
    mock_secrets();
    let (data, fake, wam, oid) = (tempfile::tempdir().unwrap(), Fake::start().await, fake::Wam::new(), account());
    wam.set_account(&oid, ME_NAME);
    let secret = || keyring_core::get_default_store().unwrap().build("open_mess/module/teams", &format!("{oid}/refresh-token"), None).unwrap();
    secret().set_password("rt-0").unwrap();
    let (module, _events) = crate::load(&fake, &wam, data.path(), None, None).await;
    module.restore(&oid).await.unwrap();
    module.sign_out(&oid).await.unwrap();
    // WAM gave the token, as before the sign-in page existed: a refresh token in the secrets is
    // neither used nor deleted.
    assert_eq!(wam.calls().len(), 1);
    assert!(fake.requests(TOKEN_PATH).is_empty());
    assert_eq!(secret().get_password().unwrap(), "rt-0");
}
