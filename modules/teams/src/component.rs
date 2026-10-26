//! The `platform-module` world of `wit/module.wit`. Milestone F1 serves the manifest; sign-in,
//! tools and live events arrive with the network wiring in milestone F2.

// The ABI glue wit-bindgen generates (bindings and the `export!` expansion) uses `unsafe`; no
// unsafe is written by hand. Listed in AGENTS.md, "Never crash".
#[allow(unsafe_code)]
mod bindings {
    wit_bindgen::generate!({ world: "platform-module", path: "../../wit" });

    use super::Teams;
    export!(Teams);
}

use bindings::exports::open_mess::module::{events, module};
use bindings::open_mess::module::types;

struct Teams;

fn not_yet() -> types::ModuleError {
    types::ModuleError::NotSupported("milestone F2".to_string())
}

impl module::Guest for Teams {
    fn manifest() -> types::Manifest {
        let m = crate::manifest::manifest();
        types::Manifest {
            name: m.name,
            version: m.version,
            platform: m.platform,
            hosts: m.hosts,
            sign_in_hosts: m.sign_in_hosts,
            wam: m.wam.map(|w| types::WamIdentity { client_id: w.client_id, scopes: w.scopes }),
            extra_tools: m
                .extra_tools
                .into_iter()
                .map(|t| types::ToolDecl { name: t.name, description: t.description, input_schema: t.input_schema, writes: t.writes })
                .collect(),
        }
    }

    fn sign_in() -> Result<types::AccountInfo, types::ModuleError> {
        Err(not_yet())
    }

    fn restore(_account: String) -> Result<(), types::ModuleError> {
        Err(not_yet())
    }

    fn sign_out(_account: String) {}

    fn call(_account: String, _tool: String, _args_json: String) -> Result<String, types::ModuleError> {
        Err(not_yet())
    }
}

impl events::Guest for Teams {
    fn on_ws_message(_conn: u32, _text: String) {}

    fn on_ws_closed(_conn: u32, _reason: String) {}

    fn on_timer(_id: u32) {}
}
