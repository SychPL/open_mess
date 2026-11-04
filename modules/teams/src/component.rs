//! The `platform-module` world of `wit/module.wit`: the exports, handed to `api`.

// The ABI glue wit-bindgen generates (bindings and the `export!` expansion) uses `unsafe`; no
// unsafe is written by hand. Listed in AGENTS.md, "Never crash".
#[allow(unsafe_code)]
pub(crate) mod bindings {
    wit_bindgen::generate!({ world: "platform-module", path: "../../wit" });

    use super::Teams;
    export!(Teams);
}

use bindings::exports::open_mess::module::{events, module};
use bindings::open_mess::module::{host, types};

use crate::api;

struct Teams;

thread_local! {
    /// Poll rounds so far, which pick the account each round starts with.
    static ROUND: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
            media: m.media.map(|peers| types::MediaAccess { peers }),
        }
    }

    fn sign_in() -> Result<types::AccountInfo, types::ModuleError> {
        api::begin();
        api::sign_in()
    }

    fn restore(account: String) -> Result<(), types::ModuleError> {
        api::begin();
        api::restore(&account)?;
        crate::live::restore(&account);
        Ok(())
    }

    fn sign_out(account: String) {
        api::begin();
        crate::live::sign_out(&account);
        api::sign_out(&account);
    }

    /// Leaves the account's meeting, within the host's 5 s.
    fn end_sessions(account: String) {
        api::begin_within(std::time::Duration::from_secs(4));
        crate::live::end_sessions(&account);
    }

    fn call(account: String, tool: String, args_json: String) -> Result<String, types::ModuleError> {
        api::begin();
        crate::tools::call(&account, &tool, &args_json)
    }

    fn get_attachment(account: String, conversation: String, message: String, attachment: String) -> Result<types::Attachment, types::ModuleError> {
        api::begin();
        crate::tools::get_attachment(&account, &conversation, &message, &attachment)
    }

    fn describe_target(account: String, conversation: String, message: Option<String>) -> Result<types::Target, types::ModuleError> {
        api::begin();
        crate::tools::describe_target(&account, &conversation, message.as_deref())
    }
}

impl events::Guest for Teams {
    fn on_ws_message(conn: u32, text: String) {
        api::begin();
        crate::live::ws_message(conn, &text);
    }

    fn on_ws_closed(conn: u32, _reason: String) {
        api::begin();
        crate::live::ws_closed(conn);
    }

    fn on_rtc_data(conn: u32, messages: Vec<Vec<u8>>, dropped: u32) {
        api::begin();
        crate::live::rtc_data(conn, &messages, dropped);
    }

    fn on_rtc_state(conn: u32, state: String) {
        api::begin();
        crate::live::rtc_state(conn, &state);
    }

    /// The poll (`poll`): every signed-in account, each round starting with the next one so a
    /// slow account cannot keep the others from their turn, then the timer again.
    fn on_timer(id: u32) {
        if id != crate::poll::TIMER {
            api::begin();
            crate::live::timer_fired(id);
            return;
        }
        api::timer_fired();
        api::begin();
        let mut accounts = api::accounts();
        accounts.sort();
        let first = ROUND.get();
        ROUND.set(first.wrapping_add(1));
        let count = accounts.len();
        if count > 0 {
            accounts.rotate_left(first % count);
        }
        for account in &accounts {
            if crate::tools::poll(account).is_err() {
                host::log(1, "polling an account for new messages failed");
            }
        }
        if !api::accounts().is_empty() {
            api::polling_again();
        }
    }
}
