//! The module manifest: name, network hosts and sign-in identity (spec 8.3, 9.1).

use crate::session::{AUTHSVC_SCOPE, CHAT_SCOPE, MCPS_SCOPE, RECORDING_SCOPE, SEARCH_SCOPE};

/// The Teams desktop client id; its redirect is `https://login.microsoftonline.com/common/oauth2/nativeclient`
/// (docs/protocols/teams.md 1.1 and 1.3, spec 9.1).
pub const CLIENT_ID: &str = "1fec8e78-bce4-4aaf-ab1b-5451cc387264";

/// Mirrors `manifest` in `wit/module.wit`.
#[derive(Debug)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub platform: String,
    pub hosts: Vec<String>,
    pub sign_in_hosts: Vec<String>,
    pub wam: Option<WamIdentity>,
    pub extra_tools: Vec<ToolDecl>,
    /// `media.peers`: the address ranges of media peers.
    pub media: Option<Vec<String>>,
}

/// Mirrors `wam-identity` in `wit/module.wit`.
#[derive(Debug)]
pub struct WamIdentity {
    pub client_id: String,
    pub scopes: Vec<String>,
}

/// Mirrors `tool-decl` in `wit/module.wit`.
#[derive(Debug)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    pub input_schema: String,
    pub writes: bool,
}

/// Hosts the module talks to, by docs/protocols/teams.md section ("Live results" for the regional
/// hosts). Suffix patterns stay below a registrable domain (spec 8.3).
const HOSTS: &[&str] = &[
    // 1.3: authorization code redemption and token refresh.
    "login.microsoftonline.com",
    // 2: authsvc; 3.1: chat service AFD hosts; 5: registrar; 7: middle tier.
    "teams.microsoft.com",
    "teams.cloud.microsoft",
    "teams.live.com",
    // 5: Trouter, `go.trouter...` or regional `go-<geo>.trouter...`.
    "*.trouter.teams.microsoft.com",
    // 7: AMS images, `amsV2` (`<geo>-prod.asyncgw...`) or `ams` (default `us-api.asm.skype.com`).
    "*.asyncgw.teams.microsoft.com",
    "*.asm.skype.com",
    // 6: search.
    "substrate.office.com",
    // 7: files on the organisation's SharePoint and OneDrive (spec 9.3).
    "*.sharepoint.com",
    // 10.7: the call store and the conversation service of live captions.
    "api.flightproxy.teams.microsoft.com",
];

/// Teams media, ID 11 of Microsoft's published Microsoft 365 address list (docs 10.7): the only
/// peers of live captions' data channel (spec 2026-10-29-live-captions, 5.2).
const MEDIA_PEERS: &[&str] = &["52.112.0.0/14", "52.122.0.0/15", "2603:1063::/38"];

/// Token audiences the module asks WAM for (docs/protocols/teams.md 1.4): authsvc and middle
/// tier, chat service, search, MCPS, the recording resource the captions bot takes (docs 10.8).
/// SharePoint's audience is the organization's own origin, known only after sign-in, so it is a
/// pattern of one label (spec 2026-10-28-meetings-transcripts, 5.2).
const SCOPES: &[&str] = &[AUTHSVC_SCOPE, CHAT_SCOPE, SEARCH_SCOPE, MCPS_SCOPE, RECORDING_SCOPE, "https://*.sharepoint.com/.default"];

pub fn manifest() -> Manifest {
    let strings = |list: &[&str]| list.iter().map(|s| s.to_string()).collect();
    Manifest {
        name: "teams".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: "teams".to_string(),
        hosts: strings(HOSTS),
        sign_in_hosts: strings(&["login.microsoftonline.com"]),
        wam: Some(WamIdentity { client_id: CLIENT_ID.to_string(), scopes: strings(SCOPES) }),
        extra_tools: extra_tools(),
        media: Some(strings(MEDIA_PEERS)),
    }
}

/// The module's own tools (spec 2026-10-28-meetings-transcripts, 4, 2026-10-29-live-captions, 4,
/// and 2026-11-04-captions-archive, 4.2-4.3); the core lists them as `teams_<name>`. Only
/// `join_captions` writes.
fn extra_tools() -> Vec<ToolDecl> {
    let time = |what: &str| serde_json::json!({"type": "string", "description": what});
    let list_meetings = serde_json::json!({
        "type": "object",
        "properties": {
            "from": time("Start of the window, RFC 3339; a week ago when left out."),
            "to": time("End of the window, RFC 3339, at most 31 days after from; a day from now when left out."),
        },
        "additionalProperties": false,
    });
    let text = |what: &str| serde_json::json!({"type": "string", "description": what});
    let get_transcript = serde_json::json!({
        "type": "object",
        "properties": {
            "conversation": text("The meeting chat, 19:meeting_..., from list_meetings."),
            "ical_uid": text("The ical_uid list_meetings gave: picks one occurrence of a recurring meeting."),
            "start": time("With end: only transcripts that overlap this window, RFC 3339."),
            "end": time("With start: only transcripts that overlap this window, RFC 3339."),
            "transcript": text("A transcript from items of an earlier result; the newest when left out."),
            "cursor": {"type": "integer", "description": "next_cursor of the previous page; needs transcript and version."},
            "version": text("version of the previous page."),
            "max_bytes": {"type": "integer", "description": "Size of a page's cues as JSON, 4096-786432; 131072 when left out."},
        },
        "required": ["conversation"],
        "additionalProperties": false,
    });
    let conversation = || text("A conversation from list_calls: the call's chat thread, 19:..., or call:<id> for a call without a chat.");
    let list_calls = serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false});
    let join_captions = serde_json::json!({
        "type": "object",
        "properties": {
            "conversation": conversation(),
            "call": text("The call id from list_calls: open_mess joins only this call, not a later one in the same conversation."),
            "language": text("The captions language, BCP 47 such as pl-pl, if open_mess turns captions on; pl-pl when left out. \
                              A language the call has already is never changed."),
            "start_captions": {"type": "boolean", "description": "Whether open_mess turns captions on if no one has; true when left out."},
        },
        "required": ["conversation", "call"],
        "additionalProperties": false,
    });
    let read_captions = serde_json::json!({
        "type": "object",
        "properties": {
            "conversation": text("A conversation the session was joined with or has, from list_calls."),
            "session": text("session of the previous result; another one means the session changed."),
            "cursor": {"type": "integer", "description": "next_cursor of the previous result; needs session."},
            "max_bytes": {"type": "integer", "description": "Size of the cues as JSON, 4096-262144; 65536 when left out."},
        },
        "required": ["conversation"],
        "additionalProperties": false,
    });
    vec![
        ToolDecl {
            name: "list_meetings".to_string(),
            description: "Teams meetings of the user's calendar in a window of time, oldest first, each with its meeting chat (conversation), \
                          which read_messages reads. Declined and cancelled meetings are left out. Reading never marks anything as read."
                .to_string(),
            input_schema: list_meetings.to_string(),
            writes: false,
        },
        ToolDecl {
            name: "get_transcript".to_string(),
            description: "The Teams transcript of a meeting, a page of cues (start, end, speaker, text) at a time; for the next page pass \
                          next_cursor as cursor with transcript and version. Any meeting the user is in, of the user's own \
                          organization. Reading never marks anything as read. What people wrote in the meeting chat is part \
                          of the meeting too: read_messages on the same conversation reads it, with since a little before \
                          this occurrence's start from list_meetings, and on with before (the oldest message returned) \
                          while a read returns limit messages. A recurring meeting keeps one chat for all its occurrences, \
                          so keep the messages up to around this occurrence's end."
                .to_string(),
            input_schema: get_transcript.to_string(),
            writes: false,
        },
        ToolDecl {
            name: "list_calls".to_string(),
            description: "The Teams calls you are in now - meetings, group and 1:1 calls - from Teams' list of your calls: \
                          each with its conversation (the call's chat, or call:<id> for a call without one) and its call \
                          id for join_captions; joined_at is null for now. Reading never marks anything as read."
                .to_string(),
            input_schema: list_calls.to_string(),
            writes: false,
        },
        ToolDecl {
            name: "join_captions".to_string(),
            description: "Join the call for its live captions, and turn captions on if no one has. open_mess joins the \
                          call as a second device of your account, without audio or video, and only while you are in \
                          the call in Teams yourself; Teams counts a second device of your account in the call meanwhile. \
                          Turning captions on adds Teams' captions bot to the call, as the Teams app does - with Teams' \
                          default settings the bot that also records and transcribes, though only captions are started, \
                          in the language given when the call has none yet. Done means joining started: read_captions \
                          tells how it goes."
                .to_string(),
            input_schema: join_captions.to_string(),
            writes: true,
        },
        ToolDecl {
            name: "read_captions".to_string(),
            description: "The live captions of the call open_mess joined with join_captions: the session's state and \
                          call, whether captions are on and were ever on in the session, the final cues (at, speaker, \
                          own - said by you - and text) from cursor, how many were missed and skipped (gone before you \
                          read them), the call's chat threads when it has more than the conversation joined (someone \
                          added to a 1:1 call), and why the session ended; pass session and next_cursor of the last \
                          result to go on. What people write in the call's chat is part of the call too: read_messages \
                          on the same conversation reads it, with since this meeting's start from list_meetings or the \
                          time you joined (a recurring meeting keeps one chat for all its occurrences); while a read \
                          returns limit messages, go on with before (the oldest message returned) and the same since. \
                          An edited message keeps its time, so read from that start again for the final text."
                .to_string(),
            input_schema: read_captions.to_string(),
            writes: false,
        },
        ToolDecl {
            name: "leave_captions".to_string(),
            description: "Leaves the call open_mess joined for its live captions; only open_mess's device leaves.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"conversation": conversation()},
                "required": ["conversation"],
                "additionalProperties": false,
            })
            .to_string(),
            writes: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_names_the_desktop_identity() {
        let m = manifest();
        assert_eq!(m.name, "teams");
        assert_eq!(m.wam.as_ref().map(|w| w.client_id.as_str()), Some("1fec8e78-bce4-4aaf-ab1b-5451cc387264"));
        assert!(m.sign_in_hosts.iter().any(|h| h == "login.microsoftonline.com"));
    }

    #[test]
    fn extra_tools_have_object_schemas_and_only_the_join_writes() {
        for tool in manifest().extra_tools {
            assert_eq!(tool.writes, tool.name == "join_captions", "{}", tool.name);
            let schema: serde_json::Value = serde_json::from_str(&tool.input_schema).unwrap();
            assert_eq!(
                (&schema["type"], &schema["additionalProperties"]),
                (&serde_json::json!("object"), &serde_json::json!(false)),
                "{}",
                tool.name
            );
            assert!(schema["properties"].get("account").is_none(), "{}", tool.name);
        }
    }

    #[test]
    fn joining_names_its_call_and_says_it_turns_captions_on() {
        let m = manifest();
        assert!(m.wam.unwrap().scopes.iter().any(|s| s == "4580fd1d-e5a3-4f56-9ad1-aab0e3bf8f76/.default"));
        let join = m.extra_tools.iter().find(|t| t.name == "join_captions").unwrap();
        // The first sentence is the approval's title (spec 2026-10-29-live-captions, 5.1).
        assert!(join.description.starts_with("Join the call for its live captions, and turn captions on if no one has. "));
        assert!(join.description.contains("captions bot"));
        let schema: serde_json::Value = serde_json::from_str(&join.input_schema).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["conversation", "call"]));
        assert!(m.extra_tools.iter().any(|t| t.name == "list_calls" && !t.writes));
    }

    #[test]
    fn host_patterns_sit_below_a_registrable_domain() {
        for h in manifest().hosts {
            if let Some(rest) = h.strip_prefix("*.") {
                assert!(rest.split('.').count() >= 2, "{h}");
                assert!(!["com", "net", "co.uk"].contains(&rest), "{h}");
            }
        }
    }
}
