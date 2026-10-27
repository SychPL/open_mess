//! The shared tools (spec 8.5) on Teams: the core's arguments in, its result shapes out
//! (`crates/open-mess/src/tools/shared.rs`). Other people's text goes under `untrusted` only.

use serde_json::{Value, json};

use crate::api::{self, Service};
use crate::chat::{self, Conversation, ConversationKind};
use crate::component::bindings::open_mess::module::types::ModuleError;
use crate::session::Session;
use crate::time;

/// Conversation pages a listing reads at most, newest first: 1000 conversations at the page size
/// of 50. ponytail: a "mark as unread" on a conversation past them is not seen; page further if
/// that ever matters.
const MAX_PAGES: usize = 20;

pub fn call(account: &str, tool: &str, args_json: &str) -> Result<String, ModuleError> {
    let args: Value = serde_json::from_str(args_json).map_err(|_| ModuleError::InvalidArgs("the arguments are not JSON".to_string()))?;
    let run: fn(&mut Session, &Value) -> Result<Value, ModuleError> = match tool {
        "list_conversations" => list_conversations,
        _ => return Err(ModuleError::NotSupported(tool.to_string())),
    };
    api::with_session(account, |session| run(session, &args)).map(|result| result.to_string())
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn limit(args: &Value, default: usize) -> usize {
    args.get("limit").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(default)
}

fn kind_name(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Channel => "channel",
        ConversationKind::Meeting => "meeting",
        // Old `@thread.skype` chats and the self chat: the core knows four kinds.
        ConversationKind::Group | ConversationKind::Other => "group",
    }
}

/// Walks the conversation list, newest first, while `keep` returns true, within `MAX_PAGES` and
/// the call's time.
fn conversations(session: &mut Session, mut keep: impl FnMut(&Session, Conversation) -> bool) -> Result<(), ModuleError> {
    let mut link: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let request = chat::list_conversations(&session.directory, link.as_deref()).map_err(api::proto)?;
        let page = chat::decode_conversations(&api::read(session, Service::Chat, &request)?).map_err(api::proto)?;
        let empty = page.items.is_empty();
        for conversation in page.items {
            if !keep(session, conversation) {
                return Ok(());
            }
        }
        match page.backward_link {
            Some(next) if !empty && api::has_time() => link = Some(next),
            _ => return Ok(()),
        }
    }
    Ok(())
}

fn list_conversations(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let kind = text(args, "kind");
    let query = text(args, "query").map(str::to_lowercase);
    let limit = limit(args, 50);
    let mut items = Vec::new();
    conversations(session, |_, c| {
        let title_matches = query.as_deref().is_none_or(|q| c.title.as_deref().is_some_and(|t| t.to_lowercase().contains(q)));
        if kind.is_none_or(|k| k == kind_name(c.kind)) && title_matches {
            // Absent values go as null, which the core reads as left out.
            let last_activity = c.horizons.last_message_ms.map(time::rfc3339);
            items.push(json!({"id": c.id, "kind": kind_name(c.kind), "last_activity": last_activity, "untrusted": {"title": c.title}}));
        }
        items.len() < limit
    })?;
    Ok(json!({ "items": items }))
}
