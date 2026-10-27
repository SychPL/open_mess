//! The chat service: conversation and message page decoders, and request builders
//! (docs/protocols/teams.md 3 and "Live results" 3). Auth headers are added by the caller (F2).

use serde_json::{Value, json};
use url::Url;

use crate::authsvc::ServiceDirectory;
use crate::http::HttpRequest;
use crate::text::escape_html;
use crate::{ProtoError, incompatible, parse_json, time};

const VIEW_CONVERSATIONS: &str = "msnp24Equivalent";
const VIEW_MESSAGES: &str = "msnp24Equivalent|supportsMessageProperties";
/// The service caps page sizes itself (docs 3.2); the real limit is still to be measured.
const CONVERSATION_PAGE_SIZE: u16 = 50;
/// The only property open_mess writes (spec 9.2).
const WRITABLE_PROPERTY: &str = "emotions";

static NULL: Value = Value::Null;

/// One page of a list. No `Debug` on pages, conversations and messages: they carry message text
/// and user ids, which never reach a log.
pub struct Page<T> {
    pub items: Vec<T>,
    /// `_metadata.backwardLink`: pass it back as `page` for the next, older page.
    pub backward_link: Option<String>,
    pub sync_state: Option<String>,
    /// `messagetype`s the decoder does not know, each once; their items were left out.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    Direct,
    Group,
    Channel,
    Meeting,
    Other,
}

pub struct Conversation {
    pub id: String,
    pub kind: ConversationKind,
    pub title: Option<String>,
    pub last_message_id: Option<String>,
    /// The last message when it is a text message the decoder reads; a conversation is listed
    /// even when its last message is not.
    pub last_message: Option<Message>,
    pub horizons: Horizons,
}

/// The read-state inputs of one conversation (docs 3.4). `unread::state` turns them into a state.
pub struct Horizons {
    /// `properties.consumptionhorizon`; `None` when the field is missing.
    pub consumption: Option<Horizon>,
    /// The "marked as unread" bookmark, `properties.consumptionHorizonBookmark` (or
    /// `userConsumptionHorizon`).
    pub bookmark: Option<Horizon>,
    /// `lastMessage.originalarrivaltime`; the wire has no `lastMessageTimeUtc` ("Live results" 3).
    pub last_message_ms: Option<i64>,
    /// `lastMessage.from`, a contact URL that ends with the sender's MRI.
    pub last_message_from: Option<String>,
    /// The `<initiator>` of a call event in `lastMessage.content`.
    pub last_call_initiator: Option<String>,
    /// `threadProperties.lastjoinat`: when the user joined the chat.
    pub joined_ms: Option<i64>,
    /// `threadProperties.createdat`: when the thread was created.
    pub created_ms: Option<i64>,
}

/// `<originalArrivalTime>;<timeStamp>;<clientMessageId>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Horizon {
    pub arrival_ms: i64,
    pub timestamp_ms: i64,
    pub client_message_id: String,
}

pub struct Message {
    pub id: String,
    pub conversation: String,
    /// The sender's MRI.
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub sent_ms: i64,
    pub edited: bool,
    pub deleted: bool,
    /// Teams HTML; plain `Text` messages are escaped into it.
    pub html: String,
    pub mentions: Vec<Mention>,
    /// The root of the reply chain the message answers in (`rootMessageId`).
    pub reply_to: Option<String>,
    /// AMS object ids of its images (`amsreferences`, docs 3.3).
    pub attachments: Vec<String>,
}

/// A member of a thread, as `describe-target` shows them.
pub struct Member {
    /// The member's MRI.
    pub id: String,
    pub name: Option<String>,
}

pub struct Mention {
    pub mri: String,
    pub name: Option<String>,
}

pub fn decode_conversations(body: &[u8]) -> Result<Page<Conversation>, ProtoError> {
    let v = parse_json(body)?;
    let items =
        array(&v, "conversations")?.iter().enumerate().map(|(i, c)| conversation(c, &format!("conversations[{i}]"))).collect::<Result<_, _>>()?;
    page(&v, items, Vec::new())
}

/// One message (`GET .../messages/<id>`); only text messages are decoded.
pub fn decode_message(body: &[u8]) -> Result<Message, ProtoError> {
    let v = parse_json(body)?;
    match req_str(&v, "", "messagetype")? {
        kind @ ("RichText/Html" | "Text") => message(&v, "", kind == "Text"),
        _ => Err(incompatible("messagetype")),
    }
}

/// One conversation (`GET .../conversations/<id>`), in the list's shape.
pub fn decode_conversation(body: &[u8]) -> Result<Conversation, ProtoError> {
    conversation(&parse_json(body)?, "")
}

/// `members[]` of `GET v1/threads/<id>/members`: `id`, and `friendlyName` when the service gives
/// one (UNCONFIRMED shape, to be checked live).
pub fn decode_members(body: &[u8]) -> Result<Vec<Member>, ProtoError> {
    let v = parse_json(body)?;
    let member = |(i, m): (usize, &Value)| {
        let path = format!("members[{i}]");
        let name = opt_str(m, &path, "friendlyName")?.filter(|n| !n.is_empty()).map(str::to_string);
        Ok(Member { id: req_str(m, &path, "id")?.to_string(), name })
    };
    array(&v, "members")?.iter().enumerate().map(member).collect()
}

pub fn decode_messages(body: &[u8]) -> Result<Page<Message>, ProtoError> {
    let v = parse_json(body)?;
    let mut items = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for (i, m) in array(&v, "messages")?.iter().enumerate() {
        let path = format!("messages[{i}]");
        match req_str(m, &path, "messagetype")? {
            kind @ ("RichText/Html" | "Text") => items.push(message(m, &path, kind == "Text")?),
            other => {
                if !skipped.iter().any(|s| s == other) {
                    skipped.push(other.to_string());
                }
            }
        }
    }
    page(&v, items, skipped)
}

fn conversation(c: &Value, path: &str) -> Result<Conversation, ProtoError> {
    let id = req_str(c, path, "id")?;
    let (props, props_path) = (obj(c, path, "properties")?, at(path, "properties"));
    let (thread, thread_path) = (obj(c, path, "threadProperties")?, at(path, "threadProperties"));
    let (last, last_path) = (obj(c, path, "lastMessage")?, at(path, "lastMessage"));
    let horizon_at = |key: &str| -> Result<Option<Horizon>, ProtoError> {
        opt_str(props, &props_path, key)?.map(|text| horizon(text, &at(&props_path, key))).transpose()
    };
    let bookmark = match horizon_at("consumptionHorizonBookmark")? {
        Some(bookmark) => Some(bookmark),
        None => horizon_at("userConsumptionHorizon")?,
    };
    let call = opt_str(last, &last_path, "messagetype")?.is_some_and(|t| t.starts_with("Event/Call"));
    let last_call_initiator = if call { opt_str(last, &last_path, "content")?.and_then(initiator) } else { None };
    let last_message = match opt_str(last, &last_path, "messagetype")? {
        Some(kind @ ("RichText/Html" | "Text")) => message(last, &last_path, kind == "Text").ok(),
        _ => None,
    };
    Ok(Conversation {
        id: id.to_string(),
        kind: kind(id),
        title: opt_str(thread, &thread_path, "topic")?.map(str::to_string),
        last_message_id: opt_str(last, &last_path, "id")?.map(str::to_string),
        last_message,
        horizons: Horizons {
            consumption: horizon_at("consumptionhorizon")?,
            bookmark,
            last_message_ms: opt_time(last, &last_path, "originalarrivaltime")?,
            last_message_from: opt_str(last, &last_path, "from")?.map(str::to_string),
            last_call_initiator,
            joined_ms: opt_time(thread, &thread_path, "lastjoinat")?,
            created_ms: opt_time(thread, &thread_path, "createdat")?,
        },
    })
}

/// The kind follows the id shapes of docs 3.1; 1:1 chats are confirmed live ("Live results" 5).
/// `@thread.skype` is used by old channels and old group chats alike, so it stays `Other`.
pub fn kind(id: &str) -> ConversationKind {
    if id.starts_with("19:meeting_") {
        ConversationKind::Meeting
    } else if id.ends_with("@unq.gbl.spaces") {
        ConversationKind::Direct
    } else if id.ends_with("@thread.tacv2") {
        ConversationKind::Channel
    } else if id.ends_with("@thread.v2") {
        ConversationKind::Group
    } else {
        ConversationKind::Other
    }
}

/// An empty horizon is `0;0;0` (docs 3.4); anything else must have all three fields.
fn horizon(text: &str, path: &str) -> Result<Horizon, ProtoError> {
    if text.is_empty() {
        return Ok(Horizon { arrival_ms: 0, timestamp_ms: 0, client_message_id: "0".to_string() });
    }
    let mut parts = text.splitn(3, ';');
    let arrival = parts.next().and_then(|p| p.parse().ok());
    let timestamp = parts.next().and_then(|p| p.parse().ok());
    match (arrival, timestamp, parts.next()) {
        (Some(arrival_ms), Some(timestamp_ms), Some(id)) => Ok(Horizon { arrival_ms, timestamp_ms, client_message_id: id.to_string() }),
        _ => Err(incompatible(path)),
    }
}

fn initiator(content: &str) -> Option<String> {
    let (_, rest) = content.split_once("<initiator>")?;
    let (mri, _) = rest.split_once("</initiator>")?;
    Some(mri.trim().to_string())
}

fn message(m: &Value, path: &str, plain: bool) -> Result<Message, ProtoError> {
    let id = req_str(m, path, "id")?;
    let from = req_str(m, path, "from")?;
    let content = req_str(m, path, "content")?;
    let (props, props_path) = (obj(m, path, "properties")?, at(path, "properties"));
    let present = |key: &str| props.get(key).is_some_and(|v| !v.is_null());
    Ok(Message {
        id: id.to_string(),
        conversation: req_str(m, path, "conversationid")?.to_string(),
        sender_id: from.rsplit('/').next().unwrap_or(from).to_string(),
        sender_name: opt_str(m, path, "imdisplayname")?.map(str::to_string),
        sent_ms: opt_time(m, path, "originalarrivaltime")?.ok_or_else(|| incompatible(&at(path, "originalarrivaltime")))?,
        edited: present("edittime"),
        deleted: present("deletetime"),
        html: if plain { escape_html(content) } else { content.to_string() },
        mentions: mentions(props, &props_path)?,
        reply_to: opt_str(m, path, "rootMessageId")?.filter(|root| *root != id).map(str::to_string),
        attachments: ams_references(m, path)?,
    })
}

fn ams_references(m: &Value, path: &str) -> Result<Vec<String>, ProtoError> {
    let path = at(path, "amsreferences");
    match m.get("amsreferences") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(ids)) => ids.iter().map(|id| id.as_str().map(str::to_string).ok_or_else(|| incompatible(&path))).collect(),
        Some(_) => Err(incompatible(&path)),
    }
}

fn mentions(props: &Value, path: &str) -> Result<Vec<Mention>, ProtoError> {
    let path = at(path, "mentions");
    let parsed;
    let list = match props.get("mentions") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(text)) if text.is_empty() => return Ok(Vec::new()),
        // The client stores the list as JSON text inside the property.
        Some(Value::String(text)) => {
            parsed = serde_json::from_str::<Value>(text).map_err(|_| incompatible(&path))?;
            &parsed
        }
        Some(list) => list,
    };
    let list = list.as_array().ok_or_else(|| incompatible(&path))?;
    let mention = |(i, m): (usize, &Value)| {
        let at = format!("{path}[{i}]");
        Ok(Mention { mri: req_str(m, &at, "mri")?.to_string(), name: opt_str(m, &at, "displayName")?.map(str::to_string) })
    };
    list.iter().enumerate().map(mention).collect()
}

fn page<T>(v: &Value, items: Vec<T>, skipped: Vec<String>) -> Result<Page<T>, ProtoError> {
    let meta = obj(v, "", "_metadata")?;
    let link = |key| Ok::<_, ProtoError>(opt_str(meta, "_metadata", key)?.map(str::to_string));
    Ok(Page { items, backward_link: link("backwardLink")?, sync_state: link("syncState")?, skipped })
}

fn at(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_string() } else { format!("{path}.{key}") }
}

fn array<'a>(v: &'a Value, key: &str) -> Result<&'a [Value], ProtoError> {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).ok_or_else(|| incompatible(key))
}

/// `v.key` as an object; absent or null reads as an empty one (every lookup in it is `None`).
pub(crate) fn obj<'a>(v: &'a Value, path: &str, key: &str) -> Result<&'a Value, ProtoError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(&NULL),
        Some(o @ Value::Object(_)) => Ok(o),
        Some(_) => Err(incompatible(&at(path, key))),
    }
}

/// `v.key` as text: `None` when absent or null, an error for any other type.
pub(crate) fn opt_str<'a>(v: &'a Value, path: &str, key: &str) -> Result<Option<&'a str>, ProtoError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(incompatible(&at(path, key))),
    }
}

pub(crate) fn req_str<'a>(v: &'a Value, path: &str, key: &str) -> Result<&'a str, ProtoError> {
    opt_str(v, path, key)?.ok_or_else(|| incompatible(&at(path, key)))
}

/// `v.key` as epoch milliseconds: a number, digits in a string, or ISO 8601 UTC text.
fn opt_time(v: &Value, path: &str, key: &str) -> Result<Option<i64>, ProtoError> {
    let ms = match v.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok().or_else(|| time::iso8601_ms(s)),
        Some(_) => None,
    };
    ms.map(Some).ok_or_else(|| incompatible(&at(path, key)))
}

/// The conversations updated since `start_ms` (1 for all of them), newest first.
pub fn list_conversations(dir: &ServiceDirectory, page: Option<&str>, start_ms: i64) -> Result<HttpRequest, ProtoError> {
    let first = format!("view={VIEW_CONVERSATIONS}&pageSize={CONVERSATION_PAGE_SIZE}&startTime={start_ms}");
    Ok(request("GET", paged(dir, endpoint(dir, &[]), page, &first)?, None))
}

/// Messages newest first; with `end_ms`, those before it (the client's `endTime`, docs 3.2; its
/// meaning is to be confirmed live, so callers still filter by time).
pub fn read_messages(dir: &ServiceDirectory, conv: &str, page: Option<&str>, limit: u16, end_ms: Option<i64>) -> Result<HttpRequest, ProtoError> {
    let end = end_ms.map(|ms| format!("&endTime={ms}")).unwrap_or_default();
    let first = format!("view={VIEW_MESSAGES}&pageSize={limit}&startTime=1{end}");
    Ok(request("GET", paged(dir, endpoint(dir, &[conv, "messages"]), page, &first)?, None))
}

pub fn one_message(dir: &ServiceDirectory, conv: &str, message_id: &str) -> HttpRequest {
    let mut url = endpoint(dir, &[conv, "messages", message_id]);
    url.set_query(Some(&format!("view={VIEW_MESSAGES}")));
    request("GET", url, None)
}

pub fn one_conversation(dir: &ServiceDirectory, conv: &str) -> HttpRequest {
    let mut url = endpoint(dir, &[conv]);
    url.set_query(Some(&format!("view={VIEW_CONVERSATIONS}")));
    request("GET", url, None)
}

/// `<chat service>/v1/threads/<id>/members` (docs 3.2).
pub fn members(dir: &ServiceDirectory, conv: &str) -> HttpRequest {
    let mut url = dir.chat_service.clone();
    // Never fails for https URLs, the only ones `authsvc::decode` accepts.
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(["v1", "threads", conv, "members"]);
    }
    url.set_query(Some(&format!("view={VIEW_CONVERSATIONS}")));
    request("GET", url, None)
}

/// The body of the send accepted live ("Live results" 5); `name` is the user's display name.
pub fn send(dir: &ServiceDirectory, conv: &str, html: &str, client_message_id: &str, name: &str) -> HttpRequest {
    let properties = json!({"importance": "", "subject": "", "mentions": "[]", "files": "[]", "formatVariant": "TEAMS"});
    let body = json!({
        "content": html,
        "messagetype": "RichText/Html",
        "contenttype": "Text",
        "imdisplayname": name,
        "clientmessageid": client_message_id,
        "properties": properties,
    });
    request("POST", endpoint(dir, &[conv, "messages"]), Some(body))
}

pub fn edit(dir: &ServiceDirectory, conv: &str, message_id: &str, html: &str) -> HttpRequest {
    let body = json!({"content": html, "messagetype": "RichText/Html", "contenttype": "Text"});
    request("PUT", endpoint(dir, &[conv, "messages", message_id]), Some(body))
}

/// Adds a reaction; the value is the reaction time (docs 3.2). Goes through `property_write`.
pub fn react(dir: &ServiceDirectory, conv: &str, message_id: &str, emotion: &str, now_ms: i64) -> Result<HttpRequest, ProtoError> {
    property_write(dir, conv, message_id, "emotions", &json!({"key": emotion, "value": now_ms}))
}

/// The client's property builder takes any name and read state is written through it, so names
/// are allow-listed, not block-listed: only `emotions`, exactly (spec 9.2).
pub fn property_write(dir: &ServiceDirectory, conv: &str, message_id: &str, name: &str, value: &Value) -> Result<HttpRequest, ProtoError> {
    if name != WRITABLE_PROPERTY {
        return Err(ProtoError::Forbidden(name.to_string()));
    }
    let mut url = endpoint(dir, &[conv, "messages", message_id, "properties"]);
    url.set_query(Some("name=emotions"));
    Ok(request("PUT", url, Some(json!({ "emotions": value }))))
}

/// `<chat service>/v1/users/ME/conversations/<segments>`. Each id is one percent-encoded path
/// segment, so `/`, `?` and `#` in it can't change the path, and `.` / `..` are dropped.
fn endpoint(dir: &ServiceDirectory, segments: &[&str]) -> Url {
    let mut url = dir.chat_service.clone();
    // Never fails for https URLs, the only ones `authsvc::decode` accepts.
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(["v1", "users", "ME", "conversations"]).extend(segments);
    }
    url
}

/// The first page's query, or the paging query of a `backwardLink`. A link may name the chat
/// service's regional host instead of the AFD one; a link to any other host is refused, and only
/// the query of an accepted one is used, on our own host and path, so a link can't send the
/// request and its token elsewhere.
fn paged(dir: &ServiceDirectory, mut url: Url, page: Option<&str>, first_page: &str) -> Result<Url, ProtoError> {
    let query = match page {
        None => first_page.to_string(),
        Some(link) => {
            let refused = || incompatible("_metadata.backwardLink");
            let link = Url::parse(link).map_err(|_| refused())?;
            let same = |base: &Url| link.host_str() == base.host_str() && link.port() == base.port();
            if link.scheme() != "https" || !(same(&url) || dir.chat_regional.as_ref().is_some_and(same)) {
                return Err(refused());
            }
            link.query().ok_or_else(refused)?.to_string()
        }
    };
    url.set_query(Some(&query));
    Ok(url)
}

fn request(method: &str, url: Url, body: Option<Value>) -> HttpRequest {
    let mut headers = vec![("BehaviorOverride".to_string(), "redirectAs404".to_string())];
    if body.is_some() {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
    HttpRequest { method: method.to_string(), url: url.into(), headers, body }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "8:orgid:00000000-0000-0000-0000-000000000001";
    const GROUP: &str = "19:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@thread.v2";
    const CHANNEL: &str = "19:cccccccccccccccccccccccccccccccc@thread.tacv2";
    const BASE: &str = "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations";

    fn dir() -> ServiceDirectory {
        let chat_service = Url::parse("https://teams.microsoft.com/api/chatsvc/emea").unwrap();
        let chat_regional = Some(Url::parse("https://emea.ng.msg.teams.microsoft.com").unwrap());
        ServiceDirectory { chat_service, chat_regional, trouter: None, registrar: None, ams: None, middle_tier: None, search: None }
    }

    fn conversations() -> Page<Conversation> {
        decode_conversations(include_bytes!("../tests/fixtures/conversations_page.json")).unwrap()
    }

    fn messages() -> Page<Message> {
        decode_messages(include_bytes!("../tests/fixtures/messages_page.json")).unwrap()
    }

    fn body(r: &HttpRequest) -> Value {
        serde_json::from_slice(&r.body).unwrap()
    }

    #[test]
    fn decodes_conversation_page() {
        let page = conversations();
        let group = &page.items[0];
        assert_eq!(group.id, GROUP);
        assert_eq!(group.last_message_id.as_deref(), Some("1800000000100"));
        let last = group.last_message.as_ref().unwrap();
        assert_eq!((last.sender_name.as_deref(), last.html.as_str()), (Some("Synthetic Person Two"), "<p>synthetic text</p>"));
        // An empty last message, or one that is not text, is none.
        assert!(page.items[1].last_message.is_none() && page.items[3].last_message.is_none());
        let consumption = Horizon { arrival_ms: 1_800_000_000_000, timestamp_ms: 1_800_000_000_050, client_message_id: "1111".to_string() };
        assert_eq!(group.horizons.consumption, Some(consumption));
        assert_eq!(group.horizons.bookmark.as_ref().map(|b| b.timestamp_ms), Some(1_800_000_000_060));
        assert!(group.horizons.last_message_from.as_deref().unwrap().ends_with("8:orgid:00000000-0000-0000-0000-000000000002"));
        // An empty horizon means 0;0;0 (docs 3.4); a missing one stays missing.
        let zero = Horizon { arrival_ms: 0, timestamp_ms: 0, client_message_id: "0".to_string() };
        assert_eq!(page.items[1].horizons.consumption, Some(zero));
        assert_eq!(page.items[3].horizons.consumption, None);
        assert_eq!(page.backward_link.as_deref(), Some(&*format!("{BASE}?view=msnp24Equivalent&pageSize=4&startTime=1&syncState=YmFja3dhcmQ")));
        assert_eq!(page.sync_state.as_deref(), Some("c3luY3N0YXRl"));
        assert!(page.skipped.is_empty());
    }

    #[test]
    fn kinds_from_id_shapes_unconfirmed_shape() {
        let kinds: Vec<_> = conversations().items.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, [ConversationKind::Group, ConversationKind::Direct, ConversationKind::Channel, ConversationKind::Meeting]);
        assert_eq!(kind("19:dddddddddddddddddddddddddddddddd@thread.skype"), ConversationKind::Other);
        assert_eq!(kind("48:notes"), ConversationKind::Other);
    }

    #[test]
    fn thread_properties_unconfirmed_shape() {
        let group = &conversations().items[0];
        assert_eq!(group.title.as_deref(), Some("Synthetic group chat"));
        assert_eq!((group.horizons.joined_ms, group.horizons.created_ms), (Some(1_799_999_000_000), Some(1_799_990_000_000)));
    }

    #[test]
    fn arrival_times_unconfirmed_shape() {
        // ISO 8601 text and epoch milliseconds both decode.
        let page = conversations();
        assert_eq!(page.items[0].horizons.last_message_ms, Some(1_800_000_000_100));
        assert_eq!(page.items[2].horizons.last_message_ms, Some(1_800_000_000_200));
    }

    #[test]
    fn call_initiator_unconfirmed_shape() {
        let conv = |kind: &str| {
            let content = format!("<partlist type=\"ended\"></partlist><initiator>{ME}</initiator>");
            let body = json!({"conversations": [{"id": GROUP, "lastMessage": {"messagetype": kind, "content": content}}]});
            decode_conversations(body.to_string().as_bytes()).unwrap().items.remove(0)
        };
        assert_eq!(conv("Event/Call").horizons.last_call_initiator.as_deref(), Some(ME));
        // Only a call event names an initiator; a text that looks like one does not count.
        assert_eq!(conv("Text").horizons.last_call_initiator, None);
    }

    #[test]
    fn mistyped_horizon_is_incompatible() {
        let body = json!({"conversations": [{"id": GROUP, "properties": {"consumptionhorizon": "soon"}}]});
        let err = decode_conversations(body.to_string().as_bytes()).err();
        assert_eq!(err, Some(ProtoError::Incompatible("conversations[0].properties.consumptionhorizon".to_string())));
    }

    #[test]
    fn decodes_message_page_with_paging_links_unconfirmed_shape() {
        let page = messages();
        assert_eq!(page.items.len(), 3);
        let m = &page.items[0];
        assert_eq!((m.id.as_str(), m.conversation.as_str()), ("1800000000300", CHANNEL));
        assert_eq!(m.sender_id, "8:orgid:00000000-0000-0000-0000-000000000002");
        assert_eq!(m.sender_name.as_deref(), Some("Synthetic Person Two"));
        assert_eq!(m.sent_ms, 1_800_000_000_300);
        assert!(m.html.starts_with("<p>Hi <span"));
        // A plain-text message is escaped so that it reads as text, not markup.
        assert_eq!(page.items[1].html, "plain 1 &lt; 2");
        assert_eq!(page.items[1].sender_id, ME);
        assert!(page.backward_link.as_deref().unwrap().ends_with("syncState=b2xkZXI"));
        assert_eq!(page.sync_state.as_deref(), Some("bmV3ZXI"));
    }

    #[test]
    fn mentions_unconfirmed_shape() {
        let m = &messages().items[0];
        assert_eq!(m.mentions.len(), 1);
        assert_eq!((m.mentions[0].mri.as_str(), m.mentions[0].name.as_deref()), (ME, Some("Synthetic Person One")));
    }

    #[test]
    fn edit_and_delete_markers_unconfirmed_shape() {
        let flags: Vec<_> = messages().items.iter().map(|m| (m.edited, m.deleted)).collect();
        assert_eq!(flags, [(false, false), (true, false), (false, true)]);
    }

    #[test]
    fn reply_root_unconfirmed_shape() {
        let page = messages();
        assert_eq!(page.items[0].reply_to.as_deref(), Some("1800000000100"));
        // A root that is the message itself, or none at all, is not a reply.
        assert_eq!((page.items[1].reply_to.as_deref(), page.items[2].reply_to.as_deref()), (None, None));
    }

    #[test]
    fn unknown_message_type_is_skipped() {
        let page = decode_messages(include_bytes!("../tests/fixtures/messages_page_unknown_type.json")).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, "1800000000300");
        assert_eq!(page.skipped, ["ThreadActivity/SyntheticNewEvent"]);
    }

    #[test]
    fn missing_message_id_is_incompatible() {
        let body = json!({"messages": [{"messagetype": "Text", "conversationid": GROUP, "from": ME, "content": "x", "originalarrivaltime": 1}]});
        let err = decode_messages(body.to_string().as_bytes()).err();
        assert_eq!(err, Some(ProtoError::Incompatible("messages[0].id".to_string())));
    }

    #[test]
    fn first_and_next_page_requests() {
        let since = list_conversations(&dir(), None, 1_800_000_000_000).unwrap();
        assert!(since.url.ends_with("&startTime=1800000000000"));
        let first = list_conversations(&dir(), None, 1).unwrap();
        assert_eq!((first.method.as_str(), first.url.as_str()), ("GET", &*format!("{BASE}?view=msnp24Equivalent&pageSize=50&startTime=1")));
        let before = read_messages(&dir(), GROUP, None, 20, Some(1_800_000_000_000)).unwrap();
        assert!(before.url.ends_with("&startTime=1&endTime=1800000000000"), "{}", before.url);
        let first = read_messages(&dir(), GROUP, None, 20, None).unwrap();
        assert_eq!(first.url, format!("{BASE}/{GROUP}/messages?view=msnp24Equivalent|supportsMessageProperties&pageSize=20&startTime=1"));
        // The next page keeps the link's paging query on our own host and path.
        let link = messages().backward_link.unwrap();
        let next = read_messages(&dir(), GROUP, Some(&link), 20, None).unwrap();
        assert!(next.url.starts_with(&format!("{BASE}/{GROUP}/messages?startTime=1&")), "{}", next.url);
        assert!(next.url.ends_with("syncState=b2xkZXI"));
        assert!(matches!(read_messages(&dir(), GROUP, Some("not a link"), 20, None), Err(ProtoError::Incompatible(_))));
        let one = one_message(&dir(), GROUP, "1800000000300");
        assert_eq!(one.url, format!("{BASE}/{GROUP}/messages/1800000000300?view=msnp24Equivalent|supportsMessageProperties"));
    }

    #[test]
    fn conversation_and_members_requests() {
        assert_eq!(one_conversation(&dir(), GROUP).url, format!("{BASE}/{GROUP}?view=msnp24Equivalent"));
        let r = members(&dir(), GROUP);
        assert_eq!(
            (r.method.as_str(), r.url.as_str()),
            ("GET", &*format!("https://teams.microsoft.com/api/chatsvc/emea/v1/threads/{GROUP}/members?view=msnp24Equivalent"))
        );
    }

    #[test]
    fn decodes_members_unconfirmed_shape() {
        let body = json!({"members": [{"id": ME, "role": "Admin", "friendlyName": "Synthetic Person One"}, {"id": "8:orgid:2", "friendlyName": ""}]});
        let members = decode_members(body.to_string().as_bytes()).unwrap();
        let pairs: Vec<_> = members.iter().map(|m| (m.id.as_str(), m.name.as_deref())).collect();
        assert_eq!(pairs, [(ME, Some("Synthetic Person One")), ("8:orgid:2", None)]);
        for bad in [json!({}), json!({"members": [{"role": "User"}]}), json!({"members": [{"id": 1}]})] {
            assert!(matches!(decode_members(bad.to_string().as_bytes()), Err(ProtoError::Incompatible(_))), "{bad}");
        }
    }

    #[test]
    fn backward_link_to_another_host_is_refused() {
        for link in [
            "https://evil.example/api/chatsvc/emea/v1/users/ME/conversations?syncState=x",
            "http://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations?syncState=x",
            "https://teams.microsoft.com:8443/api/chatsvc/emea/v1/users/ME/conversations?syncState=x",
            "https://amer.ng.msg.teams.microsoft.com/v1/users/ME/conversations?syncState=x",
        ] {
            assert!(matches!(list_conversations(&dir(), Some(link), 1), Err(ProtoError::Incompatible(_))), "{link}");
        }
    }

    #[test]
    fn single_message_with_ams_references_unconfirmed_shape() {
        let body = json!({"id": "1", "conversationid": GROUP, "from": ME, "messagetype": "RichText/Html", "content": "<p>x</p>",
                          "originalarrivaltime": 1, "amsreferences": ["0-weu-d1-a", "0-weu-d1-b"]});
        let m = decode_message(body.to_string().as_bytes()).unwrap();
        assert_eq!(m.attachments, ["0-weu-d1-a", "0-weu-d1-b"]);
        let event = json!({"id": "1", "messagetype": "ThreadActivity/AddMember", "content": ""});
        assert!(matches!(decode_message(event.to_string().as_bytes()), Err(ProtoError::Incompatible(_))));
        let bad = json!({"id": "1", "conversationid": GROUP, "from": ME, "messagetype": "Text", "content": "x", "originalarrivaltime": 1,
                         "amsreferences": [1]});
        assert!(matches!(decode_message(bad.to_string().as_bytes()), Err(ProtoError::Incompatible(p)) if p == "amsreferences"));
    }

    #[test]
    fn send_carries_client_message_id() {
        let r = send(&dir(), GROUP, "<p>hi</p>", "9999", "Synthetic Person One");
        assert_eq!((r.method.as_str(), r.url.as_str()), ("POST", &*format!("{BASE}/{GROUP}/messages")));
        let properties = json!({"importance": "", "subject": "", "mentions": "[]", "files": "[]", "formatVariant": "TEAMS"});
        let expected = json!({"content": "<p>hi</p>", "messagetype": "RichText/Html", "contenttype": "Text", "imdisplayname": "Synthetic Person One",
                              "clientmessageid": "9999", "properties": properties});
        assert_eq!(body(&r), expected);
        assert!(r.headers.contains(&("BehaviorOverride".to_string(), "redirectAs404".to_string())));
    }

    #[test]
    fn edit_targets_the_message() {
        let r = edit(&dir(), GROUP, "1800000000300", "<p>fixed</p>");
        assert_eq!((r.method.as_str(), r.url.as_str()), ("PUT", &*format!("{BASE}/{GROUP}/messages/1800000000300")));
        assert_eq!(body(&r)["content"], "<p>fixed</p>");
    }

    #[test]
    fn ids_stay_inside_their_path_segment() {
        let r = edit(&dir(), "../x?name=consumptionhorizon#", "a/b", "");
        assert_eq!(r.url, format!("{BASE}/..%2Fx%3Fname=consumptionhorizon%23/messages/a%2Fb"));
    }

    #[test]
    fn react_writes_only_emotions() {
        let r = react(&dir(), GROUP, "1800000000300", "like", 1_800_000_000_500).unwrap();
        assert_eq!((r.method.as_str(), r.url.as_str()), ("PUT", &*format!("{BASE}/{GROUP}/messages/1800000000300/properties?name=emotions")));
        assert_eq!(body(&r), json!({"emotions": {"key": "like", "value": 1_800_000_000_500_i64}}));
    }

    #[test]
    fn consumption_horizon_write_is_forbidden() {
        for name in ["consumptionhorizon", "ConsumptionHorizon"] {
            let r = property_write(&dir(), GROUP, "1", name, &json!("1;2;3"));
            assert!(matches!(r, Err(ProtoError::Forbidden(n)) if n == name), "{name}");
        }
    }

    #[test]
    fn only_emotions_is_writable() {
        // Every write of docs/protocols/teams.md 3.4, and near misses of the one allowed name.
        let refused = [
            "consumptionhorizon",
            "consumptionHorizonBookmark",
            "userConsumptionHorizon",
            "isread",
            "userMetaData",
            "l2ch",
            "rclch",
            "rclchBookmark",
            "appConsumptionHorizon",
            "Emotions",
            "EMOTIONS",
            "emotions ",
            "",
        ];
        for name in refused {
            assert!(matches!(property_write(&dir(), GROUP, "1", name, &json!("x")), Err(ProtoError::Forbidden(_))), "{name}");
        }
        assert!(property_write(&dir(), GROUP, "1", "emotions", &json!({"key": "like"})).is_ok());
    }
}
