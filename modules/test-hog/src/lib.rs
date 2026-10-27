//! A test guest for `crates/open-mess-host` whose `call` allocates 1 MiB blocks and keeps them,
//! for the memory limit.

#[cfg(target_arch = "wasm32")]
mod guest {
    // The ABI glue wit-bindgen generates (bindings and the `export!` expansion) uses `unsafe`; no
    // unsafe is written by hand. Listed in AGENTS.md, "Never crash".
    #[allow(unsafe_code)]
    mod bindings {
        wit_bindgen::generate!({ world: "platform-module", path: "../../wit" });

        use super::Hog;
        export!(Hog);
    }

    use bindings::exports::open_mess::module::{events, module};
    use bindings::open_mess::module::types;

    struct Hog;

    impl module::Guest for Hog {
        fn manifest() -> types::Manifest {
            types::Manifest {
                name: "test-hog".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                platform: "test".to_string(),
                hosts: vec!["localhost".to_string()],
                sign_in_hosts: Vec::new(),
                wam: None,
                extra_tools: Vec::new(),
            }
        }

        fn sign_in() -> Result<types::AccountInfo, types::ModuleError> {
            Err(types::ModuleError::NotSupported("sign-in".to_string()))
        }

        fn restore(_account: String) -> Result<(), types::ModuleError> {
            Ok(())
        }

        fn sign_out(_account: String) {}

        fn get_attachment(
            _account: String,
            _conversation: String,
            _message: String,
            _attachment: String,
        ) -> Result<types::Attachment, types::ModuleError> {
            Err(types::ModuleError::NotSupported("get-attachment".to_string()))
        }

        fn call(_account: String, _tool: String, _args_json: String) -> Result<String, types::ModuleError> {
            let mut kept = Vec::new();
            loop {
                kept.push(vec![1u8; 1 << 20]);
            }
        }

        fn describe_target(_account: String, _conversation: String, _message: Option<String>) -> Result<types::Target, types::ModuleError> {
            Err(types::ModuleError::NotSupported("describe-target".to_string()))
        }
    }

    impl events::Guest for Hog {
        fn on_ws_message(_conn: u32, _text: String) {}

        fn on_ws_closed(_conn: u32, _reason: String) {}

        fn on_timer(_id: u32) {}
    }
}
