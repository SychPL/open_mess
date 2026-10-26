//! A test guest for `crates/open-mess-host` whose `call` loops forever, for the call time limit.

#[cfg(target_arch = "wasm32")]
mod guest {
    // The ABI glue wit-bindgen generates (bindings and the `export!` expansion) uses `unsafe`; no
    // unsafe is written by hand. Listed in AGENTS.md, "Never crash".
    #[allow(unsafe_code)]
    mod bindings {
        wit_bindgen::generate!({ world: "platform-module", path: "../../wit" });

        use super::Spin;
        export!(Spin);
    }

    use bindings::exports::open_mess::module::{events, module};
    use bindings::open_mess::module::types;

    struct Spin;

    impl module::Guest for Spin {
        fn manifest() -> types::Manifest {
            types::Manifest {
                name: "test-spin".to_string(),
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
            loop {
                std::hint::spin_loop();
            }
        }
    }

    impl events::Guest for Spin {
        fn on_ws_message(_conn: u32, _text: String) {}

        fn on_ws_closed(_conn: u32, _reason: String) {}

        fn on_timer(_id: u32) {}
    }
}
