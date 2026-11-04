//! The module manifest: name, network hosts and sign-in identity (spec 8.3, 9.1).

use crate::session::{AUTHSVC_SCOPE, CHAT_SCOPE, MCPS_SCOPE, SEARCH_SCOPE};

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
/// tier, chat service, search, MCPS. SharePoint's audience is the organization's own origin, known
/// only after sign-in, so it is a pattern of one label (spec 2026-10-28-meetings-transcripts, 5.2).
const SCOPES: &[&str] = &[AUTHSVC_SCOPE, CHAT_SCOPE, SEARCH_SCOPE, MCPS_SCOPE, "https://*.sharepoint.com/.default"];

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

/// The module's own tools (spec 2026-10-28-meetings-transcripts, 4, and 2026-10-29-live-captions,
/// 4); the core lists them as `teams_<name>`. Only `join_captions` writes.
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
    let read_captions = serde_json::json!({
        "type": "object",
        "properties": {
            "conversation": text("The meeting chat, 19:meeting_..., the session was joined with."),
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
            name: "join_captions".to_string(),
            description: "Join the meeting for its live captions. open_mess joins the meeting as a second device of your \
                          account, without audio or video, only to read the captions someone turned on in Teams, and \
                          only while you are in the meeting in Teams yourself; Teams counts a second device of your \
                          account in the meeting meanwhile. Done means joining started: read_captions tells how it goes."
                .to_string(),
            input_schema: meeting(&text).to_string(),
            writes: true,
        },
        ToolDecl {
            name: "read_captions".to_string(),
            description: "The live captions of the meeting open_mess joined with join_captions: the session's state, \
                          the final cues (at, speaker, text) from cursor, how many were missed, and why the session \
                          ended; pass session and next_cursor of the last result to go on. What people write in the \
                          meeting chat is part of the meeting too: read_messages on the same conversation reads it, with \
                          since this meeting's start from list_meetings or the time you joined (a recurring meeting keeps \
                          one chat for all its occurrences); while a read returns limit messages, go on with before (the \
                          oldest message returned) and the same since. An edited message keeps its time, so read from \
                          that start again for the final text."
                .to_string(),
            input_schema: read_captions.to_string(),
            writes: false,
        },
        ToolDecl {
            name: "leave_captions".to_string(),
            description: "Leaves the meeting open_mess joined for its live captions; only open_mess's device leaves.".to_string(),
            input_schema: meeting(&text).to_string(),
            writes: false,
        },
    ]
}

/// The schema of a tool that takes only the meeting chat.
fn meeting(text: &impl Fn(&str) -> serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {"conversation": text("The meeting chat, 19:meeting_..., from list_meetings or list_conversations.")},
        "required": ["conversation"],
        "additionalProperties": false,
    })
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
    fn host_patterns_sit_below_a_registrable_domain() {
        for h in manifest().hosts {
            if let Some(rest) = h.strip_prefix("*.") {
                assert!(rest.split('.').count() >= 2, "{h}");
                assert!(!["com", "net", "co.uk"].contains(&rest), "{h}");
            }
        }
    }
}
