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
];

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
    }
}

/// The module's own tools, all reads (spec 2026-10-28-meetings-transcripts, 4); the core lists
/// them as `teams_<name>`.
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
                          next_cursor as cursor with transcript and version. For now only recordings the user owns. Reading never marks \
                          anything as read."
                .to_string(),
            input_schema: get_transcript.to_string(),
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
    fn extra_tools_are_reads_with_object_schemas() {
        for tool in manifest().extra_tools {
            assert!(!tool.writes, "{}", tool.name);
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
