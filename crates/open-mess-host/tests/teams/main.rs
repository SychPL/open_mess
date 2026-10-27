//! The built Teams module in the real host, against a fake Teams server on loopback and a fake
//! WAM (docs/protocols/teams.md; spec 9). No test signs in anywhere or reaches Microsoft.

// clippy.toml allows unwrap, panic and indexing in #[test] functions only; the helpers are test
// code too.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

#[path = "../common/mod.rs"]
mod common;
mod fake;

use std::path::Path;

use fake::{AUTHZ_PATH, Fake, ME_NAME, ME_OID, Wam, WamCall};
use open_mess_host::ip_policy::AddressPolicy;
use open_mess_host::{AccountInfo, Engine, HostError, HostEvent, ModuleContext, ModuleError, ModuleHandle, Net, limits};
use tokio::sync::mpsc;

const AUTHSVC_SCOPE: &str = "https://api.spaces.skype.com/.default";

/// The Teams module, loaded with its hosts served by `fake` and its tokens from `wam`.
struct Teams {
    module: ModuleHandle,
    fake: Fake,
    wam: Wam,
    _data: tempfile::TempDir,
}

impl Teams {
    async fn start() -> Teams {
        let data = tempfile::tempdir().unwrap();
        let (fake, wam) = (Fake::start().await, Wam::new());
        let (module, _events) = load(&fake, &wam, data.path()).await;
        Teams { module, fake, wam, _data: data }
    }
}

async fn load(fake: &Fake, wam: &Wam, data_dir: &Path) -> (ModuleHandle, mpsc::Receiver<HostEvent>) {
    let engine = Engine::with_limits(limits::for_tests()).unwrap().with_fake_wam(wam.hook());
    let net = Net::with_limits(AddressPolicy::AllowLoopbackForTests, limits::for_tests()).serving_hosts_from(fake.addr);
    let (events, receiver) = mpsc::channel(64);
    let module = engine.load(&common::wasm("open_mess_teams"), ModuleContext { data_dir: data_dir.to_path_buf(), events, net }).await.unwrap();
    (module, receiver)
}

fn module_error(result: Result<impl std::fmt::Debug, HostError>) -> ModuleError {
    match result {
        Err(HostError::Module(e)) => e,
        other => panic!("expected a module error, got {other:?}"),
    }
}

// Task 1: sign-in and restore.

#[tokio::test]
async fn sign_in_returns_the_account_from_the_token() {
    let teams = Teams::start().await;
    let account = teams.module.sign_in().await.unwrap();
    assert_eq!(account, AccountInfo { id: ME_OID.to_string(), display_name: ME_NAME.to_string() });
    assert_eq!(teams.wam.calls(), [WamCall { scopes: vec![AUTHSVC_SCOPE.to_string()], interactive: true }]);
    // The account is checked with Teams itself: authsvc got the token, on its own host.
    let authz = teams.fake.requests(AUTHZ_PATH);
    assert_eq!(authz.len(), 1);
    assert_eq!((authz[0].method.as_str(), authz[0].host.as_str()), ("POST", "teams.microsoft.com"));
    assert_eq!(authz[0].audience().as_deref(), Some("https://api.spaces.skype.com"));
}

#[tokio::test]
async fn sign_in_refused_by_teams_is_auth_required() {
    let teams = Teams::start().await;
    teams.fake.on(|_| Some(fake::Reply::status(403)));
    assert!(matches!(module_error(teams.module.sign_in().await), ModuleError::AuthRequired(_)));
}

#[tokio::test]
async fn restore_gets_a_new_token_silently() {
    let teams = Teams::start().await;
    teams.module.restore(ME_OID).await.unwrap();
    assert_eq!(teams.wam.calls(), [WamCall { scopes: vec![AUTHSVC_SCOPE.to_string()], interactive: false }]);
    assert_eq!(teams.fake.requests(AUTHZ_PATH).len(), 1);
}

#[tokio::test]
async fn restore_without_wam_session_is_auth_required() {
    let teams = Teams::start().await;
    teams.wam.set_silent(false);
    assert!(matches!(module_error(teams.module.restore(ME_OID).await), ModuleError::AuthRequired(_)));
    // Never interactive outside sign-in, and nothing was sent anywhere.
    assert!(teams.wam.calls().iter().all(|call| !call.interactive));
    assert!(teams.fake.seen().is_empty());
}

#[tokio::test]
async fn restore_of_another_windows_account_is_auth_required() {
    let teams = Teams::start().await;
    teams.wam.set_account("00000000-0000-0000-0000-000000000009", "Synthetic Someone Else");
    assert!(matches!(module_error(teams.module.restore(ME_OID).await), ModuleError::AuthRequired(_)));
    assert!(teams.fake.seen().is_empty());
}
