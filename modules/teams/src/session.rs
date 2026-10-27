//! A signed-in account as the module keeps it: tokens and the service directory, in the module
//! instance's memory only, never in storage or secrets (spec 9.1: with WAM the broker holds the
//! session, and a new instance gets new tokens silently).

use crate::authsvc::ServiceDirectory;
use crate::http::SecretString;

/// Token audiences (docs/protocols/teams.md 1.4 and "Live results" 1-3): authsvc and the middle
/// tier, the chat service (a plain AAD token is accepted, "Live results" 3), search.
pub const AUTHSVC_SCOPE: &str = "https://api.spaces.skype.com/.default";
pub const CHAT_SCOPE: &str = "https://ic3.teams.office.com/.default";
pub const SEARCH_SCOPE: &str = "https://outlook.office.com/.default";

/// A token is renewed this long before it expires.
const RENEW_BEFORE_MS: i64 = 5 * 60 * 1000;

/// What the module reads from a token. The token came from WAM, not from the network, so its
/// signature is not checked; the claims only name the account it belongs to.
#[derive(Debug, PartialEq, Eq)]
pub struct Claims {
    /// The user's object id: the account id and, as `8:orgid:<oid>`, the user's MRI.
    pub oid: String,
    pub name: Option<String>,
    pub upn: Option<String>,
    pub tenant: Option<String>,
    pub expires_ms: Option<i64>,
}

/// The claims of a JWT, `None` when it is not one or has no `oid`.
pub fn claims(token: &str) -> Option<Claims> {
    let payload = token.split('.').nth(1)?;
    let v: serde_json::Value = serde_json::from_slice(&base64_decode(payload)?).ok()?;
    let text = |key: &str| v.get(key).and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    Some(Claims {
        oid: text("oid")?,
        name: text("name"),
        upn: text("upn").or_else(|| text("unique_name")),
        tenant: text("tid"),
        expires_ms: v.get("exp").and_then(serde_json::Value::as_i64).map(|secs| secs.saturating_mul(1000)),
    })
}

/// The claims a 401's `WWW-Authenticate` asks for (CAE, docs/protocols/teams.md 1.3:
/// `Bearer ..., error="insufficient_claims", claims="<base64 JSON>"`), decoded; `None` without any.
pub fn challenge_claims(www_authenticate: &str) -> Option<String> {
    const KEY: &str = "claims=\"";
    // Lowercasing ASCII keeps every byte offset.
    let start = www_authenticate.to_ascii_lowercase().find(KEY)? + KEY.len();
    let value = www_authenticate.get(start..)?.split('"').next()?;
    let decoded = base64_decode(value).and_then(|bytes| String::from_utf8(bytes).ok());
    Some(decoded.unwrap_or_else(|| value.to_string())).filter(|claims| !claims.is_empty())
}

/// Base64 in either alphabet (standard or URL-safe), padding optional.
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut bits, mut count) = (0u32, 0u32);
    for c in text.trim_end_matches('=').bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

pub struct Session {
    /// The account id: the user's object id.
    pub account: String,
    pub name: String,
    pub upn: Option<String>,
    pub tenant: Option<String>,
    pub directory: ServiceDirectory,
    /// Scope, token and when it expires.
    tokens: Vec<(&'static str, SecretString, Option<i64>)>,
}

impl Session {
    pub fn new(claims: Claims, directory: ServiceDirectory) -> Session {
        let name = claims.name.or_else(|| claims.upn.clone()).unwrap_or_else(|| claims.oid.clone());
        Session { account: claims.oid, name, upn: claims.upn, tenant: claims.tenant, directory, tokens: Vec::new() }
    }

    /// The user's MRI, as the chat service names senders and mentions.
    pub fn me(&self) -> String {
        format!("8:orgid:{}", self.account)
    }

    /// A token for `scope` that is not about to expire.
    pub fn token(&self, scope: &str, now_ms: i64) -> Option<&str> {
        let (_, token, expires) = self.tokens.iter().find(|(s, ..)| *s == scope)?;
        let fresh = expires.is_none_or(|at| at.saturating_sub(RENEW_BEFORE_MS) > now_ms);
        fresh.then(|| token.expose())
    }

    pub fn keep(&mut self, scope: &'static str, token: String, expires_ms: Option<i64>) {
        self.tokens.retain(|(s, ..)| *s != scope);
        self.tokens.push((scope, SecretString::new(token), expires_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    /// `{"oid":"o-1","name":"Synthetic Person","upn":"p@synthetic.example","tid":"t-1","exp":1800000000}`
    const TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJvaWQiOiJvLTEiLCJuYW1lIjoiU3ludGhldGljIFBlcnNvbiIsInVwbiI6InBAc3ludGhldGljLmV4YW1wbGUiLCJ0aWQiOiJ0LTEiLCJleHAiOjE4MDAwMDAwMDB9.c2ln";

    fn session() -> Session {
        let directory = ServiceDirectory {
            chat_service: Url::parse("https://teams.microsoft.com/api/chatsvc/emea").unwrap(),
            chat_regional: None,
            trouter: None,
            registrar: None,
            ams: None,
            middle_tier: None,
            search: None,
        };
        Session::new(claims(TOKEN).unwrap(), directory)
    }

    #[test]
    fn claims_name_the_account() {
        let c = claims(TOKEN).unwrap();
        assert_eq!(c.oid, "o-1");
        assert_eq!((c.name.as_deref(), c.upn.as_deref(), c.tenant.as_deref()), (Some("Synthetic Person"), Some("p@synthetic.example"), Some("t-1")));
        assert_eq!(c.expires_ms, Some(1_800_000_000_000));
        assert_eq!(session().me(), "8:orgid:o-1");
    }

    #[test]
    fn not_a_token_has_no_claims() {
        // `{"name":"x"}` has no oid.
        for bad in ["", "abc", "a.b!c.d", "a.eyJuYW1lIjoieCJ9.c", "a.bm90IGpzb24.c"] {
            assert_eq!(claims(bad), None, "{bad}");
        }
    }

    #[test]
    fn base64_takes_both_alphabets_and_padding() {
        assert_eq!(base64_decode("aGk/Pz8-"), Some(b"hi???>".to_vec()));
        assert_eq!(base64_decode("aGk_Pz8+"), Some(b"hi???>".to_vec()));
        assert_eq!(base64_decode("aGk="), Some(b"hi".to_vec()));
        assert_eq!(base64_decode("a b"), None);
    }

    #[test]
    fn challenge_claims_are_decoded() {
        // base64 of {"access_token":{"nbf":{"essential":true}}}
        let header = r#"Bearer realm="", authorization_uri="https://login.example/", error="insufficient_claims", Claims="eyJhY2Nlc3NfdG9rZW4iOnsibmJmIjp7ImVzc2VudGlhbCI6dHJ1ZX19fQ==""#;
        assert_eq!(challenge_claims(header).as_deref(), Some(r#"{"access_token":{"nbf":{"essential":true}}}"#));
        // Plain JSON is passed on as it is; no claims, or empty ones, are none.
        assert_eq!(challenge_claims(r#"Bearer claims="{x}""#).as_deref(), Some("{x}"));
        for none in ["Bearer realm=\"\"", "Bearer claims=\"\"", "", "claims=\""] {
            assert_eq!(challenge_claims(none), None, "{none}");
        }
    }

    #[test]
    fn tokens_are_renewed_before_they_expire() {
        let mut s = session();
        assert_eq!(s.token(CHAT_SCOPE, 0), None);
        s.keep(CHAT_SCOPE, "t1".to_string(), Some(1_000_000));
        assert_eq!(s.token(CHAT_SCOPE, 1_000_000 - RENEW_BEFORE_MS - 1), Some("t1"));
        assert_eq!(s.token(CHAT_SCOPE, 1_000_000 - RENEW_BEFORE_MS), None);
        assert_eq!(s.token(SEARCH_SCOPE, 0), None);
        s.keep(CHAT_SCOPE, "t2".to_string(), None);
        assert_eq!(s.token(CHAT_SCOPE, i64::MAX), Some("t2"));
    }
}
