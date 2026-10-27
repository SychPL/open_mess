//! The authsvc exchange: an Entra token for `https://api.spaces.skype.com` in, the skype token and
//! the service directory (`regionGtms`) out (docs/protocols/teams.md 2 and "Live results" 2).

use serde_json::Value;
use url::Url;

use crate::http::{HttpRequest, SecretString};
use crate::{ProtoError, incompatible, parse_json};

const AUTHZ_URL: &str = "https://teams.microsoft.com/api/authsvc/v1.0/authz";

/// The signed-in session as authsvc describes it.
#[derive(Debug)]
pub struct Session {
    /// Still needed by the middle tier; the chat service takes the Entra token instead.
    pub skype_token: Option<SecretString>,
    pub expires_at_ms: Option<i64>,
    pub directory: ServiceDirectory,
}

/// Base URLs of the services, from `regionGtms`. Only https URLs are accepted.
#[derive(Debug)]
pub struct ServiceDirectory {
    /// `chatServiceAfd`, required.
    pub chat_service: Url,
    /// `calling_trouterUrl`.
    pub trouter: Option<Url>,
    /// `calling_registrarUrl`.
    pub registrar: Option<Url>,
    /// `amsV2`, else `ams`.
    pub ams: Option<Url>,
    /// `middleTier`.
    pub middle_tier: Option<Url>,
    /// `search` (the live response has no `searchService`).
    pub search: Option<Url>,
}

/// The authz request with the headers the live check sent (docs/protocols/teams.md "Live
/// results" 2), less the bearer token for `https://api.spaces.skype.com`, which the caller adds.
/// The authz type is `ExplicitLogin`; the client sends `TokenRefresh` on a refresh, which F2b adds
/// if the service needs it.
pub fn request() -> HttpRequest {
    let headers =
        [("claimsChallengeCapable", "true"), ("ms-teams-authz-type", "ExplicitLogin"), ("X-MS-Migration", "True"), ("x-ms-client-type", "web")]
            .map(|(name, value)| (name.to_string(), value.to_string()));
    HttpRequest { method: "POST".to_string(), url: AUTHZ_URL.to_string(), headers: headers.to_vec(), body: Vec::new() }
}

/// Decodes the authz response. `expiresIn` is in seconds, counted from `now_ms`.
pub fn decode(body: &[u8], now_ms: i64) -> Result<Session, ProtoError> {
    let v = parse_json(body)?;
    let gtms = v.get("regionGtms").ok_or_else(|| incompatible("regionGtms"))?;
    let url = |key: &str| -> Result<Option<Url>, ProtoError> {
        let Some(value) = gtms.get(key) else { return Ok(None) };
        let path = format!("regionGtms.{key}");
        let url = value.as_str().and_then(|s| Url::parse(s).ok()).filter(|u| u.scheme() == "https");
        url.map(Some).ok_or(ProtoError::Incompatible(path))
    };
    let directory = ServiceDirectory {
        chat_service: url("chatServiceAfd")?.ok_or_else(|| incompatible("regionGtms.chatServiceAfd"))?,
        trouter: url("calling_trouterUrl")?,
        registrar: url("calling_registrarUrl")?,
        ams: match url("amsV2")? {
            Some(ams) => Some(ams),
            None => url("ams")?,
        },
        middle_tier: url("middleTier")?,
        search: url("search")?,
    };

    let (skype_token, expires_at_ms) = match v.get("tokens") {
        None => (None, None),
        Some(tokens) => {
            let token = tokens.get("skypeToken").and_then(Value::as_str).ok_or_else(|| incompatible("tokens.skypeToken"))?;
            let expires_in = match tokens.get("expiresIn") {
                None => None,
                Some(secs) => Some(secs.as_i64().ok_or_else(|| incompatible("tokens.expiresIn"))?),
            };
            (Some(SecretString::new(token.to_string())), expires_in.map(|secs| now_ms.saturating_add(secs.saturating_mul(1000))))
        }
    };
    Ok(Session { skype_token, expires_at_ms, directory })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &[u8] = include_bytes!("../tests/fixtures/authsvc_ok.json");
    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn decodes_region_directory() {
        let s = decode(OK, NOW).unwrap();
        assert_eq!(s.directory.chat_service.as_str(), "https://teams.microsoft.com/api/chatsvc/emea");
        assert_eq!(s.directory.ams.as_ref().map(Url::as_str), Some("https://eu-prod.asyncgw.teams.microsoft.com/"));
        assert_eq!(s.directory.search.as_ref().map(Url::as_str), Some("https://teams.microsoft.com/api/search/emea"));
        assert_eq!(s.skype_token.as_ref().map(SecretString::expose), Some("synthetic-skype-token-not-a-real-one"));
        assert_eq!(s.expires_at_ms, Some(NOW + 4_500_000));
    }

    #[test]
    fn missing_chat_service_is_incompatible() {
        let body = include_bytes!("../tests/fixtures/authsvc_missing_chat.json");
        match decode(body, NOW) {
            Err(ProtoError::Incompatible(p)) => assert!(p.contains("chatServiceAfd"), "{p}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn ams_falls_back_and_skype_token_is_optional() {
        let body = br#"{"regionGtms": {"chatServiceAfd": "https://teams.microsoft.com/api/chatsvc/emea", "ams": "https://eu-api.asm.skype.com"}}"#;
        let s = decode(body, NOW).unwrap();
        assert_eq!(s.directory.ams.as_ref().map(Url::as_str), Some("https://eu-api.asm.skype.com/"));
        assert!(s.skype_token.is_none() && s.expires_at_ms.is_none());
    }

    #[test]
    fn plain_http_directory_is_incompatible() {
        let body =
            String::from_utf8(OK.to_vec()).unwrap().replace("https://teams.microsoft.com/api/chatsvc", "http://teams.microsoft.com/api/chatsvc");
        assert!(matches!(decode(body.as_bytes(), NOW), Err(ProtoError::Incompatible(_))));
    }

    #[test]
    fn debug_never_prints_token() {
        let s = decode(OK, NOW).unwrap();
        assert!(!format!("{s:?}").contains("synthetic-skype-token"));
    }

    #[test]
    fn request_posts_to_authz() {
        let r = request();
        assert_eq!((r.method.as_str(), r.url.as_str()), ("POST", "https://teams.microsoft.com/api/authsvc/v1.0/authz"));
        assert!(r.headers.contains(&("X-MS-Migration".to_string(), "True".to_string())));
        assert!(r.body.is_empty());
    }
}
