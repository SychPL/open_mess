//! The shared tools (spec 8.5) on Teams: the core's arguments in, its result shapes out
//! (`crates/open-mess/src/tools/shared.rs`). Other people's text goes under `untrusted` only.
//! Nothing writes read state: the reads are GETs (and search's query POST), and the writes are a
//! message POST or PUT and the `emotions` property, the only one `chat::property_write` allows
//! (spec 9.2).

use serde_json::{Value, json};

use crate::api::{self, Service};
use crate::chat::{self, Conversation, ConversationKind, Message};
use crate::component::bindings::open_mess::module::types::{Attachment, ModuleError, Participant, Target};
use crate::component::bindings::open_mess::module::{host, storage};
use crate::http::HttpRequest;
use crate::poll::PollState;
use crate::session::Session;
use crate::text::{html_to_markdown, markdown_to_html};
use crate::unread::{self, UnreadState};
use crate::writing::{self, Purpose};
use crate::{search, time};

/// Pages a listing reads at most, newest first: 1000 conversations or messages at the page size
/// of 50. ponytail: a "mark as unread" on a conversation past them is not seen; page further if
/// that ever matters.
const MAX_PAGES: usize = 20;
const PAGE_SIZE: u16 = 50;
/// `mentions` reads the messages of at most this many recently active conversations.
const MENTION_CONVERSATIONS: usize = 20;
/// `mentions` without `since` looks back this far.
const MENTIONS_DEFAULT_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// The wrapper's `writing_context` looks at the last 100 messages.
const WRITING_MESSAGES: usize = 100;

pub fn call(account: &str, tool: &str, args_json: &str) -> Result<String, ModuleError> {
    let args: Value = serde_json::from_str(args_json).map_err(|_| ModuleError::InvalidArgs("the arguments are not JSON".to_string()))?;
    let run: fn(&mut Session, &Value) -> Result<Value, ModuleError> = match tool {
        "list_conversations" => list_conversations,
        "unread" => unread,
        "mentions" => mentions,
        "read_messages" => read_messages,
        "search" => search,
        "writing_context" => writing_context,
        "send" => send,
        "edit_message" => edit_message,
        "react" => react,
        _ => return Err(ModuleError::NotSupported(tool.to_string())),
    };
    api::with_session(account, |session| run(session, &args)).map(|result| result.to_string())
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, ModuleError> {
    text(args, key).ok_or_else(|| ModuleError::InvalidArgs(format!("{key} is required")))
}

fn limit(args: &Value, default: usize) -> usize {
    args.get("limit").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(default)
}

fn since(args: &Value) -> Result<Option<i64>, ModuleError> {
    text(args, "since").map(|t| time::rfc3339_ms(t).ok_or_else(|| ModuleError::InvalidArgs("since must be an RFC 3339 time".to_string()))).transpose()
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

/// Walks the conversations updated since `start_ms`, newest first, while `keep` returns true,
/// within `MAX_PAGES` and the call's time. With `partial`, a later page that fails ends the walk
/// with what the earlier ones gave (a listing); without, it is an error (unread, mentions and the
/// poll must not pass a failure off as "nothing there").
fn conversations(session: &mut Session, start_ms: i64, partial: bool, mut keep: impl FnMut(Conversation) -> bool) -> Result<(), ModuleError> {
    let mut link: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let request = chat::list_conversations(&session.directory, link.as_deref(), start_ms).map_err(api::proto)?;
        let body = match api::read(session, Service::Chat, &request) {
            Ok(body) => body,
            // A later page that fails leaves what the earlier ones found, as the page limit does;
            // a session that needs sign-in still ends the call.
            Err(e) if partial && link.is_some() && !matches!(e, ModuleError::AuthRequired(_)) => {
                host::log(1, "a later page of the conversation list failed; the list is cut short");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let page = chat::decode_conversations(&body).map_err(api::proto)?;
        let empty = page.items.is_empty();
        for conversation in page.items {
            if !keep(conversation) {
                return Ok(());
            }
        }
        match page.backward_link {
            Some(_) if !empty && !api::has_time() && !partial => {
                return Err(ModuleError::Platform("Teams answers too slowly; try again later".to_string()));
            }
            Some(next) if !empty && api::has_time() => link = Some(next),
            _ => return Ok(()),
        }
    }
    Ok(())
}

/// Walks a conversation's text messages, newest first, while `keep` returns true, within
/// `MAX_PAGES` and the call's time.
/// With `end_ms`, the walk starts at messages before then.
fn messages(session: &mut Session, conversation: &str, end_ms: Option<i64>, mut keep: impl FnMut(Message) -> bool) -> Result<(), ModuleError> {
    let mut link: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let request = chat::read_messages(&session.directory, conversation, link.as_deref(), PAGE_SIZE, end_ms).map_err(api::proto)?;
        let page = chat::decode_messages(&api::read(session, Service::Chat, &request)?).map_err(api::proto)?;
        let empty = page.items.is_empty() && page.skipped.is_empty();
        for message in page.items {
            if !keep(message) {
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

fn message_item(m: &Message, me: &str) -> Value {
    json!({
        "id": m.id,
        "conversation": m.conversation,
        "sent_at": time::rfc3339(m.sent_ms),
        "edited": m.edited,
        "own": m.sender_id == me,
        "untrusted": {"sender_name": m.sender_name, "text": html_to_markdown(&m.html)},
    })
}

/// The conversation's topic. A 1:1 chat has none: the other person's name from their last message
/// stands in; [`name_direct_chats`] names the others when a query needs it.
fn title(c: &Conversation, me: &str) -> Option<String> {
    let other = || c.last_message.as_ref().filter(|m| m.sender_id != me).and_then(|m| m.sender_name.clone());
    c.title.clone().or_else(|| if c.kind == ConversationKind::Direct { other() } else { None })
}

/// Every matching conversation within the pages read: `limit` is not applied here, because the
/// core cuts the list to it only after leaving out what the agent may not read.
fn list_conversations(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let kind = text(args, "kind");
    let query = text(args, "query").map(str::to_lowercase);
    let me = session.me();
    // A person's MRI finds the 1:1 chat with them.
    if let Some(other) = query.as_deref().and_then(|q| q.strip_prefix("8:orgid:")) {
        let found = one_to_one(session, other)?.filter(|c| kind.is_none_or(|k| k == kind_name(c.kind)));
        let items: Vec<Value> = found.iter().map(|c| conversation_item(c, title(c, &me))).collect();
        return Ok(json!({ "items": items }));
    }
    let mut found = Vec::new();
    conversations(session, 1, true, |c| {
        if kind.is_none_or(|k| k == kind_name(c.kind)) {
            let title = title(&c, &me);
            found.push((c, title));
        }
        true
    })?;
    if let Some(query) = &query {
        name_direct_chats(session, &mut found, &me)?;
        found.retain(|(_, title)| title.as_deref().is_some_and(|t| t.to_lowercase().contains(query.as_str())));
    }
    let items: Vec<Value> = found.into_iter().map(|(c, title)| conversation_item(&c, title)).collect();
    Ok(json!({ "items": items }))
}

/// 1:1 chats named from their members a query looks at, at most this many a call.
const MAX_NAMED: usize = 50;

/// Names the 1:1 chats the list leaves nameless (the user's own message is the last one) from
/// their members, newest first, within [`MAX_NAMED`] and the call's time, so a query by the other
/// person's name finds them. A chat whose members may not be read stays nameless; members in a
/// shape the module does not know end the call as incompatible (spec 9.3).
fn name_direct_chats(session: &mut Session, found: &mut [(Conversation, Option<String>)], me: &str) -> Result<(), ModuleError> {
    let nameless = found.iter_mut().filter(|(c, title)| title.is_none() && c.kind == ConversationKind::Direct);
    for (c, title) in nameless.take(MAX_NAMED) {
        if !api::has_time() {
            break;
        }
        let request = chat::members(&session.directory, &c.id);
        match api::read(session, Service::Chat, &request) {
            Ok(body) => {
                let members = chat::decode_members(&body).map_err(api::proto)?;
                *title = members.into_iter().find(|m| m.id != me).and_then(|m| m.name);
            }
            Err(e @ (ModuleError::AuthRequired(_) | ModuleError::Incompatible(_))) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(())
}

fn conversation_item(c: &Conversation, title: Option<String>) -> Value {
    // Absent values go as null, which the core reads as left out.
    let last_activity = c.horizons.last_message_ms.map(time::rfc3339);
    json!({"id": c.id, "kind": kind_name(c.kind), "last_activity": last_activity, "untrusted": {"title": title}})
}

/// The official client's rule (docs/protocols/teams.md 3.4), reading horizons only. Where the
/// account-wide "mark all as read" horizon comes from is not confirmed yet ("Live results" 4), so
/// what looks unread is reported as unknown, never as read.
fn unread(session: &mut Session, _args: &Value) -> Result<Value, ModuleError> {
    let me = session.me();
    let mut items = Vec::new();
    conversations(session, 1, false, |c| {
        // A conversation without a message has nothing to read.
        if c.last_message_id.is_none() && c.horizons.last_message_ms.is_none() {
            return true;
        }
        let state = match unread::state(&c, None, &me) {
            UnreadState::Read => return true,
            UnreadState::Unread => "unread",
            UnreadState::Unknown => "unknown",
        };
        let last_activity = c.horizons.last_message_ms.map(time::rfc3339);
        let title = title(&c, &me);
        items.push(
            json!({"conversation": c.id, "kind": kind_name(c.kind), "unread": state, "last_activity": last_activity, "untrusted": {"title": title}}),
        );
        true
    })?;
    Ok(json!({ "items": items }))
}

/// Messages that mention the user, from the [`MENTION_CONVERSATIONS`] conversations active most
/// recently since `since`; the shared tool's description tells agents so.
fn mentions(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let since = since(args)?.unwrap_or_else(|| api::now_ms().saturating_sub(MENTIONS_DEFAULT_MS));
    let me = session.me();
    let mut active = Vec::new();
    conversations(session, since, false, |c| {
        if c.horizons.last_message_ms.is_some_and(|t| t > since) {
            active.push(c.id);
        }
        active.len() < MENTION_CONVERSATIONS
    })?;
    let mut found: Vec<Message> = Vec::new();
    for conversation in active {
        if !api::has_time() {
            break;
        }
        messages(session, &conversation, None, |m| {
            if m.sent_ms <= since {
                return false;
            }
            if m.sender_id != me && !m.deleted && m.mentions.iter().any(|mention| mention.mri == me) {
                found.push(m);
            }
            true
        })?;
    }
    found.sort_by_key(|c| std::cmp::Reverse(c.sent_ms));
    Ok(json!({ "items": found.iter().map(|m| message_item(m, &me)).collect::<Vec<_>>() }))
}

fn read_messages(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = required(args, "conversation")?;
    let (limit, before, since) = (limit(args, 30), text(args, "before"), since(args)?);
    let me = session.me();
    // `before` by its time, so the history behind it is reached however deep it is.
    let before_ms = match before {
        None => None,
        Some(id) => {
            let response = api::send(session, Service::Chat, &chat::one_message(&session.directory, conversation, id), false)?;
            if response.status == 404 {
                return Err(ModuleError::InvalidArgs("the message in before was not found in the conversation".to_string()));
            }
            Some(chat::decode_message(&api::ok(response)?).map_err(api::proto)?.sent_ms)
        }
    };
    let mut items = Vec::new();
    messages(session, conversation, before_ms, |m| {
        if before_ms.is_some_and(|b| m.sent_ms >= b) {
            return true;
        }
        if since.is_some_and(|s| m.sent_ms <= s) {
            return false;
        }
        if !m.deleted {
            items.push(message_item(&m, &me));
        }
        items.len() < limit
    })?;
    Ok(json!({ "items": items }))
}

fn search(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let query = required(args, "query")?;
    let size = u32::try_from(limit(args, 30)).unwrap_or(30);
    // `Oid:<oid>@<tenant>`, the format of the client's `addSubstrateIdHeaders` (to be confirmed live).
    let anchor = format!("Oid:{}@{}", session.account, session.tenant.as_deref().unwrap_or_default());
    let request = search::request(query, size, &anchor, &api::uuid());
    let hits = search::decode(&api::read(session, Service::Search, &request)?).map_err(api::proto)?;
    let me = session.upn.as_deref().map(str::to_lowercase);
    let items: Vec<Value> = hits
        .iter()
        .map(|hit| {
            let own = me.is_some() && hit.sender_address.as_deref().map(str::to_lowercase) == me;
            json!({
                "id": hit.id,
                "conversation": hit.conversation,
                "sent_at": time::rfc3339(hit.sent_ms),
                "edited": false,
                "own": own,
                "untrusted": {"sender_name": hit.sender_name, "text": hit.text},
            })
        })
        .collect();
    Ok(json!({ "items": items }))
}

/// Examples of the user's own messages, filtered like teams-wrapper's (spec 8.5).
fn writing_context(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = required(args, "conversation")?;
    let purpose = match text(args, "purpose") {
        None => Purpose::Casual,
        Some(name) => Purpose::parse(name).ok_or_else(|| ModuleError::InvalidArgs(format!("unknown purpose {name:?}")))?,
    };
    let me = session.me();
    let (mut seen, mut candidates) = (0, Vec::new());
    messages(session, conversation, None, |m| {
        seen += 1;
        if m.sender_id == me && !m.deleted {
            candidates.extend(writing::candidate(&m.html, m.sent_ms));
        }
        seen < WRITING_MESSAGES
    })?;
    let examples: Vec<Value> = writing::select(candidates, purpose, limit(args, 6))
        .into_iter()
        .map(|c| json!({"sent_at": time::rfc3339(c.sent_ms), "untrusted": {"text": c.text}}))
        .collect();
    Ok(json!({ "examples": examples }))
}

/// An AMS image of a message: only one the message references (docs 7).
pub fn get_attachment(account: &str, conversation: &str, message: &str, attachment: &str) -> Result<Attachment, ModuleError> {
    api::with_session(account, |session| {
        let request = chat::one_message(&session.directory, conversation, message);
        let m = chat::decode_message(&api::read(session, Service::Chat, &request)?).map_err(api::proto)?;
        if !m.attachments.iter().any(|id| id == attachment) {
            return Err(ModuleError::InvalidArgs("the message has no such attachment".to_string()));
        }
        let mut url =
            session.directory.ams.clone().ok_or_else(|| ModuleError::Platform("Teams named no media service for this account".to_string()))?;
        url.path_segments_mut()
            .map_err(|()| ModuleError::Incompatible("the media service address".to_string()))?
            .pop_if_empty()
            .extend(["v1", "objects", attachment, "views", "imgo"]);
        let request = HttpRequest { method: "GET".to_string(), url: url.into(), headers: Vec::new(), body: Vec::new() };
        let response = api::send(session, Service::Media, &request, false)?;
        let content_type = api::header(&response.headers, "content-type").unwrap_or("application/octet-stream").to_string();
        let extension = match content_type.split(';').next().unwrap_or_default().trim() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "bin",
        };
        let data = api::ok(response)?;
        Ok(Attachment { file_name: format!("{attachment}.{extension}"), content_type, data })
    })
}

/// The 1:1 chat with the person whose object id is `other`, if there is one. Its id joins both
/// object ids in an order not settled yet ("Live results" 5), so both orders are looked up; a chat
/// is never created. Only `list_conversations` uses it: the id it returns is what the core's
/// per-conversation permissions apply to, so a person's id never stands in for a conversation.
fn one_to_one(session: &mut Session, other: &str) -> Result<Option<Conversation>, ModuleError> {
    if other.is_empty() || !other.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(ModuleError::InvalidArgs("not a person's id".to_string()));
    }
    let me = session.account.clone();
    for id in [format!("19:{me}_{other}@unq.gbl.spaces"), format!("19:{other}_{me}@unq.gbl.spaces")] {
        let response = api::send(session, Service::Chat, &chat::one_conversation(&session.directory, &id), false)?;
        if response.status != 404 {
            return chat::decode_conversation(&api::ok(response)?).map(Some).map_err(api::proto);
        }
    }
    Ok(None)
}

/// The `conversation` argument of a write or a target: a conversation id. A person's id would
/// dodge the permissions set for the chat it stands for.
fn conversation_id(conversation: &str) -> Result<&str, ModuleError> {
    if conversation.starts_with("8:") {
        return Err(ModuleError::InvalidArgs(
            "a person's id is not a conversation; list_conversations with it as query finds the 1:1 chat".to_string(),
        ));
    }
    Ok(conversation)
}

/// Sends a write once (spec 8.5: one that may have been taken is `uncertain`, never repeated) and
/// returns its `Location`, if any.
fn write(session: &mut Session, request: &HttpRequest) -> Result<Option<String>, ModuleError> {
    let response = api::send(session, Service::Chat, request, true)?;
    let location = api::header(&response.headers, "location").map(str::to_string);
    api::ok(response)?;
    Ok(location)
}

/// A message, or in a channel a reply in the thread of `reply_to`. A quoted reply in a chat has no
/// confirmed format yet, so it is refused rather than sent without its quote.
fn send(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = conversation_id(required(args, "conversation")?)?;
    let target = match text(args, "reply_to") {
        None => conversation.to_string(),
        // Old channels are `@thread.skype` too, like old group chats: they are tried as a channel.
        Some(reply_to) if matches!(chat::kind(conversation), ConversationKind::Channel | ConversationKind::Other) => {
            // A reply to a reply goes into the same thread: the thread is its root's.
            let request = chat::one_message(&session.directory, conversation, reply_to);
            let replied = chat::decode_message(&api::read(session, Service::Chat, &request)?).map_err(api::proto)?;
            format!("{conversation};messageid={}", replied.reply_to.as_deref().unwrap_or(reply_to))
        }
        Some(_) => return Err(ModuleError::NotSupported("a reply to a chat message; send it without reply_to".to_string())),
    };
    let html = markdown_to_html(required(args, "text")?);
    let request = chat::send(&session.directory, &target, &html, &api::random().to_string(), &session.name);
    let location = write(session, &request)?;
    // The new message's id ends the `Location` (docs 3.2).
    let id = location.as_deref().and_then(|l| l.rsplit('/').next()).filter(|id| !id.is_empty());
    Ok(json!({ "id": id }))
}

fn edit_message(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = conversation_id(required(args, "conversation")?)?;
    let request = chat::edit(&session.directory, conversation, required(args, "message")?, &markdown_to_html(required(args, "text")?));
    write(session, &request)?;
    Ok(json!({}))
}

/// Teams' own keys for the emoji agents are likely to send; anything else goes as it is.
fn reaction_key(reaction: &str) -> &str {
    match reaction.trim_end_matches('\u{fe0f}') {
        "\u{1f44d}" => "like",
        "\u{2764}" => "heart",
        "\u{1f602}" | "\u{1f606}" => "laugh",
        "\u{1f62e}" => "surprised",
        "\u{1f622}" => "sad",
        "\u{1f620}" | "\u{1f621}" => "angry",
        _ => reaction,
    }
}

fn react(session: &mut Session, args: &Value) -> Result<Value, ModuleError> {
    let conversation = conversation_id(required(args, "conversation")?)?;
    let key = reaction_key(required(args, "reaction")?);
    let request = chat::react(&session.directory, conversation, required(args, "message")?, key, api::now_ms()).map_err(api::proto)?;
    write(session, &request)?;
    Ok(json!({}))
}

/// What a write in `conversation` is about (spec 8.6): its title, its members (none for a
/// channel, which is the target itself) and the text of `message`. Reads only.
pub fn describe_target(account: &str, conversation: &str, message: Option<&str>) -> Result<Target, ModuleError> {
    let conversation = conversation_id(conversation)?;
    api::with_session(account, |session| {
        let request = chat::one_conversation(&session.directory, conversation);
        let c = chat::decode_conversation(&api::read(session, Service::Chat, &request)?).map_err(api::proto)?;
        let participants: Vec<Participant> = if c.kind == ConversationKind::Channel {
            Vec::new()
        } else {
            let request = chat::members(&session.directory, conversation);
            let members = chat::decode_members(&api::read(session, Service::Chat, &request)?).map_err(api::proto)?;
            members.into_iter().map(|m| Participant { display_name: m.name.unwrap_or_else(|| m.id.clone()), id: m.id }).collect()
        };
        let me = session.me();
        let others = || participants.iter().filter(|p| p.id != me).map(|p| p.display_name.as_str()).collect::<Vec<_>>().join(", ");
        let conversation_title = c.title.clone().unwrap_or_else(others);
        let target_text = match message {
            None => None,
            Some(id) => {
                let request = chat::one_message(&session.directory, conversation, id);
                let m = chat::decode_message(&api::read(session, Service::Chat, &request)?).map_err(api::proto)?;
                Some(html_to_markdown(&m.html))
            }
        };
        Ok(Target { conversation_title, participants, target_text })
    })
}

/// Messages read, newest first, and the link to the next page when the pages ran out first.
type Fresh = (Vec<Message>, Option<String>);

/// A conversation's messages from `link` on (from the newest without one), newest first, page by
/// page while `fresh` holds, with the link to the next page when `MAX_PAGES` ran out first; `None`
/// when they may not be read (403, 404). Any other failure leaves them for the next poll.
fn fresh_messages(
    session: &mut Session,
    conversation: &str,
    mut link: Option<String>,
    fresh: impl Fn(&Message) -> bool,
) -> Result<Option<Fresh>, ModuleError> {
    let slow = || ModuleError::Platform("Teams answers too slowly; polling again later".to_string());
    let mut new = Vec::new();
    for page_number in 0..MAX_PAGES {
        if !api::has_time() {
            return Err(slow());
        }
        let request = chat::read_messages(&session.directory, conversation, link.as_deref(), PAGE_SIZE, None).map_err(api::proto)?;
        let response = api::send(session, Service::Chat, &request, false)?;
        if page_number == 0 && matches!(response.status, 403 | 404) {
            return Ok(None);
        }
        let page = chat::decode_messages(&api::ok(response)?).map_err(api::proto)?;
        let mut more = !(page.items.is_empty() && page.skipped.is_empty());
        for m in page.items {
            if !fresh(&m) {
                more = false;
                break;
            }
            new.push(m);
        }
        match page.backward_link {
            Some(next) if more => link = Some(next),
            _ => return Ok(Some((new, None))),
        }
    }
    Ok(Some((new, link)))
}

/// One poll of an account (`poll`): the conversations updated lately, one `message-arrived` for
/// each new message. Reads only, as background requests.
pub fn poll(account: &str) -> Result<(), ModuleError> {
    api::background();
    api::with_session(account, |session| {
        let key = crate::poll::key(account);
        let started = api::now_ms();
        // No state (it could not be read or stored at sign-in): count from one interval back.
        let fallback = || PollState::new(started.saturating_sub(i64::from(crate::poll::INTERVAL_MS)));
        let mut state = storage::get(&key).and_then(|bytes| PollState::decode(&bytes)).unwrap_or_else(fallback);
        let me = session.me();
        let mut changed = Vec::new();
        conversations(session, state.start_ms(), false, |c| {
            if state.changed(&c) {
                changed.push(c);
            }
            true
        })?;
        let behind = state.take_behind();
        let (mut arrivals, mut held) = (Vec::new(), false);
        // A conversation whose messages cannot be read now is left for the next poll, which starts
        // from the same floor; the others are reported. A list that cannot be read ends the poll.
        for c in changed {
            match fresh_messages(session, &c.id, None, |m| state.fresh(&c.id, m)) {
                Ok(Some((new, next))) => {
                    if let Some(next) = next
                        && !state.fall_behind(&c.id, next, started)
                    {
                        host::log(1, "too many conversations fell behind; the older messages of one are not reported");
                    }
                    arrivals.extend(state.arrivals(&c, Some(&new), &me));
                }
                Ok(None) => arrivals.extend(state.arrivals(&c, None, &me)),
                Err(e @ ModuleError::AuthRequired(_)) => return Err(e),
                Err(_) if state.can_hold(started) => held = true,
                Err(_) => arrivals.extend(state.arrivals(&c, None, &me)),
            }
        }
        // Older pages earlier polls left, with the time that is left: a long backlog never holds
        // back what is new. Their messages are reported after the new ones too, so that a call
        // running out of time while it reports loses old messages, not new ones. (A trigger batch
        // that also gets more than 100 old ones then lists the newest of those; it counts them all.)
        for mut b in behind {
            match fresh_messages(session, &b.conversation, Some(b.link.clone()), |m| b.fresh(m)) {
                Ok(Some((older, next))) => {
                    arrivals.extend(PollState::older_arrivals(&b, &older, &me));
                    let Some(next) = next else { continue };
                    b.link = next;
                }
                // No longer readable: nothing to report.
                Ok(None) => continue,
                Err(e @ ModuleError::AuthRequired(_)) => return Err(e),
                // Tried again by the next poll.
                Err(_) => {}
            }
            if !state.keep_behind(b, started) {
                host::log(1, "older messages of a conversation are given up: an hour passed, or too many conversations fell behind");
            }
        }
        state.finish(started, held);
        // Stored before they are reported, so a crash in between loses them rather than
        // reporting them twice.
        if storage::set(&key, &state.encode()).is_err() {
            host::log(1, "the poll state was not stored; messages may be reported again");
        }
        for a in arrivals {
            let message = host::NewMessage {
                account: account.to_string(),
                conversation: a.conversation,
                message: a.message,
                sender: a.sender,
                own: a.own,
                mentions_me: a.mentions_me,
                direct: a.direct,
            };
            host::message_arrived(&message);
        }
        Ok(())
    })
}
