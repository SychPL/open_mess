//! The module manifest: name, network hosts and sign-in identity (spec 8.3, 9.1).

use crate::session::{AUTHSVC_SCOPE, CHAT_SCOPE, SEARCH_SCOPE};

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
/// tier, chat service, search. SharePoint's audience is the tenant's own origin, known only after
/// sign-in.
const SCOPES: &[&str] = &[AUTHSVC_SCOPE, CHAT_SCOPE, SEARCH_SCOPE];

pub fn manifest() -> Manifest {
    let strings = |list: &[&str]| list.iter().map(|s| s.to_string()).collect();
    Manifest {
        name: "teams".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: "teams".to_string(),
        hosts: strings(HOSTS),
        sign_in_hosts: strings(&["login.microsoftonline.com"]),
        wam: Some(WamIdentity { client_id: CLIENT_ID.to_string(), scopes: strings(SCOPES) }),
        extra_tools: Vec::new(),
    }
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
    fn host_patterns_sit_below_a_registrable_domain() {
        for h in manifest().hosts {
            if let Some(rest) = h.strip_prefix("*.") {
                assert!(rest.split('.').count() >= 2, "{h}");
                assert!(!["com", "net", "co.uk"].contains(&rest), "{h}");
            }
        }
    }
}
