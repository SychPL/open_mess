//! Loading platform modules: the built Teams component and files that are not components.

mod common;

use common::{context, wasm};
use open_mess_host::{Engine, HostError};

#[tokio::test]
async fn loads_teams_manifest() {
    let engine = Engine::new().unwrap();
    let module = engine.load(&wasm("open_mess_teams"), context()).await.unwrap();
    let manifest = module.manifest().unwrap();
    assert_eq!(manifest.name, "teams");
    assert_eq!(manifest.wam.unwrap().client_id, "1fec8e78-bce4-4aaf-ab1b-5451cc387264");
}

#[tokio::test]
async fn rejects_non_component() {
    let path = std::env::temp_dir().join(format!("open-mess-not-a-component-{}.wasm", std::process::id()));
    std::fs::write(&path, b"\x13\x37 definitely not a webassembly component").unwrap();
    let result = Engine::new().unwrap().load(&path, context()).await;
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(result, Err(HostError::Load(_))), "{:?}", result.err());
}

#[tokio::test]
async fn rejects_a_wam_scope_pattern_with_another_path() {
    let mut bytes = std::fs::read(wasm("open_mess_test_echo")).unwrap();
    // The host pattern `*.sharepoint.com` may share the scope's bytes; the path is the scope's own.
    let (from, to) = (b"https://*.sharepoint.com/.default", b"https://*.sharepoint.com/.Default");
    let at: Vec<usize> = bytes.windows(from.len()).enumerate().filter(|(_, window)| window == from).map(|(at, _)| at).collect();
    assert_eq!(at.len(), 1, "the scope pattern must occur once in test-echo");
    bytes.splice(at[0]..at[0] + from.len(), to.iter().copied());
    let result = Engine::new().unwrap().load_bytes(bytes, context()).await;
    assert!(matches!(&result, Err(HostError::Load(why)) if why.contains("WAM scope pattern")), "{:?}", result.err());
}
