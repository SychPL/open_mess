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
