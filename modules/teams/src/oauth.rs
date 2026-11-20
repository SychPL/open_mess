//! Sign-in without WAM: authorization code with PKCE through the sign-in page, and token refresh
//! (docs/protocols/teams.md 1.3, spec 2026-11-20-teams-sign-in-without-wam, 4.3). Pure functions:
//! `api` does the network and the secrets.
//!
//! Nothing here puts a token, a code or an `error_description` into an error: the answers come
//! from the network, and the description can carry an e-mail address.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use url::{Url, form_urlencoded};

use crate::http::{HttpRequest, SecretString};
use crate::manifest::CLIENT_ID;
use crate::session::AUTHSVC_SCOPE;

/// `organizations`: work and school accounts only, like WAM (spec 2, 4).
const AUTHORIZE_URL: &str = "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize";
const TOKEN_URL: &str = "https://login.microsoftonline.com/organizations/oauth2/v2.0/token";
/// The client's native redirect: the page ends there and the host hands back that address.
pub const REDIRECT_URI: &str = "https://login.microsoftonline.com/common/oauth2/nativeclient";
/// CAE, as the official client asks (docs 1.3).
const CAE_CLAIMS: &str = r#"{"access_token":{"xms_cc":{"values":["CP1"]}}}"#;

/// Why a step failed, in the module's error kinds.
#[derive(Debug, PartialEq, Eq)]
pub enum OauthError {
    /// The user has to sign in again.
    AuthRequired(String),
    Incompatible(String),
    Platform(String),
}

/// One sign-in's secrets. No `Debug` derive on purpose: the verifier redeems the code.
pub struct Pkce {
    pub verifier: SecretString,
    pub challenge: String,
    pub state: String,
}

/// What a token answer holds.
pub struct Tokens {
    pub access: SecretString,
    /// A refresh before the old one is spent may not return a new one; the old one stays then.
    pub refresh: Option<SecretString>,
    /// Seconds.
    pub expires_in: Option<i64>,
}

/// A new verifier (32 random bytes, RFC 7636 4.1), its S256 challenge and a `state` (16 bytes).
pub fn pkce() -> Result<Pkce, OauthError> {
    let verifier = random_token::<32>()?;
    let challenge = challenge(&verifier);
    Ok(Pkce { verifier: SecretString::new(verifier), challenge, state: random_token::<16>()? })
}

/// `base64url(SHA-256(verifier))`, no padding (RFC 7636 4.2).
pub fn challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// Random bytes as base64url, from `getrandom` (the host's `wasi:random`), never a fixed value.
fn random_token<const N: usize>() -> Result<String, OauthError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| OauthError::Platform("the system gave no randomness for the sign-in".to_string()))?;
    Ok(base64url(&bytes))
}

/// Base64, URL-safe alphabet, no padding.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // The chunk's bytes left-aligned in 24 bits; a chunk of n bytes gives n + 1 characters.
        let bits = chunk.iter().fold(0u32, |bits, byte| (bits << 8) | u32::from(*byte)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            let index = ((bits >> (18 - 6 * i)) & 63) as usize;
            out.push(ALPHABET.get(index).map_or('A', |c| char::from(*c)));
        }
    }
    out
}

/// The sign-in page's address (step 1).
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let scope = format!("{AUTHSVC_SCOPE} offline_access openid profile");
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("response_mode", "query")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", &scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("prompt", "select_account")
        .append_pair("claims", CAE_CLAIMS)
        .finish();
    format!("{AUTHORIZE_URL}?{query}")
}

/// The code of the address the page ended at (step 3): for this sign-in's `state` only. An `error`
/// answer becomes its code and `AADSTS` number, and nothing else of the description.
pub fn redirect(answer: &str, state: &str) -> Result<SecretString, OauthError> {
    let auth = |why: &str| OauthError::AuthRequired(why.to_string());
    let url = Url::parse(answer).map_err(|_| auth("the sign-in answer is not a web address"))?;
    let param = |name: &str| url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned());
    if param("state").as_deref() != Some(state) {
        return Err(auth("the sign-in answer is not for this sign-in"));
    }
    if let Some(error) = param("error") {
        let description = param("error_description");
        let code = aadsts(description.as_deref().unwrap_or_default());
        return Err(OauthError::AuthRequired(format!("Microsoft refused the sign-in{}", detail(&error, code.as_deref()))));
    }
    param("code").filter(|code| !code.is_empty()).map(SecretString::new).ok_or_else(|| auth("the sign-in answer has no code"))
}

/// The token request of step 4 or 4.3.4.
pub fn token_request(body: Vec<u8>) -> HttpRequest {
    // No Origin header: the client is not a SPA registration (docs 1.3).
    let headers = vec![("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string())];
    HttpRequest { method: "POST".to_string(), url: TOKEN_URL.to_string(), headers, body }
}

/// The body that redeems the code of `redirect` (step 4).
pub fn code_form(code: &str, verifier: &str) -> Vec<u8> {
    let scope = format!("{AUTHSVC_SCOPE} offline_access openid profile");
    form(&[
        ("client_id", CLIENT_ID),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", verifier),
        ("scope", &scope),
        ("claims", CAE_CLAIMS),
    ])
}

/// The body of a refresh for `scope` (4.3.4); `claims` is the final claims, see [`merge_claims`].
pub fn refresh_form(refresh_token: &str, scope: &str, claims: &str) -> Vec<u8> {
    let scope = format!("{scope} offline_access");
    form(&[("client_id", CLIENT_ID), ("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("scope", &scope), ("claims", claims)])
}

fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish().into_bytes()
}

/// The claims of a refresh: CAE's `xms_cc`, joined with a challenge's (4.3.4). A challenge that
/// is not a JSON object with an `access_token` object, or has none, goes on as it is.
pub fn merge_claims(challenge: Option<&str>) -> String {
    let Some(challenge) = challenge else { return CAE_CLAIMS.to_string() };
    let Ok(Value::Object(mut claims)) = serde_json::from_str::<Value>(challenge) else { return challenge.to_string() };
    let access = claims.entry("access_token").or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(access) = access else { return challenge.to_string() };
    access.entry("xms_cc").or_insert_with(|| serde_json::json!({"values": ["CP1"]}));
    Value::Object(claims).to_string()
}

/// Decodes the token endpoint's answer (step 5). `need_refresh`: a sign-in without a refresh
/// token is incompatible, a refresh without one keeps the old.
pub fn token_response(status: u16, body: &[u8], need_refresh: bool) -> Result<Tokens, OauthError> {
    match status {
        200..=299 => {}
        400 | 401 => return Err(microsoft_error(status, body)),
        _ => return Err(unavailable(status)),
    }
    let incompatible = |path: &str| OauthError::Incompatible(format!("unexpected server response at {path}"));
    let v: Value = serde_json::from_slice(body).map_err(|_| incompatible("$"))?;
    let text = |key: &str| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| SecretString::new(s.to_string()));
    let access = text("access_token").ok_or_else(|| incompatible("access_token"))?;
    let refresh = text("refresh_token");
    if need_refresh && refresh.is_none() {
        return Err(incompatible("refresh_token"));
    }
    // A number, or a string on the older endpoints.
    let expires_in = v.get("expires_in").and_then(|e| e.as_i64().or_else(|| e.as_str()?.parse().ok())).filter(|secs| *secs >= 0);
    Ok(Tokens { access, refresh, expires_in })
}

/// A 400 or 401 of the token endpoint (4.3.5).
fn microsoft_error(status: u16, body: &[u8]) -> OauthError {
    let v: Option<Value> = serde_json::from_slice(body).ok();
    let Some(error) = v.as_ref().and_then(|v| v.get("error")).and_then(Value::as_str) else { return unavailable(status) };
    let number = v.as_ref().and_then(|v| v.get("error_codes")).and_then(Value::as_array).and_then(|codes| codes.first()).and_then(Value::as_u64);
    let description = v.as_ref().and_then(|v| v.get("error_description")).and_then(Value::as_str);
    let code = number.map(|n| format!("AADSTS{n}")).or_else(|| aadsts(description.unwrap_or_default()));
    match error {
        "invalid_grant" | "interaction_required" | "consent_required" | "login_required" => {
            let code = code.map(|c| format!(" ({c})")).unwrap_or_default();
            OauthError::AuthRequired(format!("Microsoft asks to sign in again{code}"))
        }
        _ => OauthError::Platform(format!("the sign-in service refused the request{}", detail(error, code.as_deref()))),
    }
}

fn unavailable(status: u16) -> OauthError {
    OauthError::Platform(format!("the sign-in service answered HTTP {status}"))
}

/// ` (<error>, <AADSTS number>)` for a message; the error only when it looks like a code, since
/// it came from the network.
fn detail(error: &str, aadsts: Option<&str>) -> String {
    let looks_like_code = !error.is_empty() && error.len() <= 64 && error.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let parts: Vec<&str> = looks_like_code.then_some(error).into_iter().chain(aadsts).collect();
    if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) }
}

/// The first `AADSTS<digits>` of an error description.
fn aadsts(text: &str) -> Option<String> {
    let rest = text.get(text.find("AADSTS")? + "AADSTS".len()..)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).take(10).collect();
    (!digits.is_empty()).then(|| format!("AADSTS{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pairs(query: &str) -> HashMap<String, String> {
        form_urlencoded::parse(query.as_bytes()).into_owned().collect()
    }

    fn body(bytes: &[u8]) -> HashMap<String, String> {
        pairs(&String::from_utf8_lossy(bytes))
    }

    fn query_of(url: &str) -> HashMap<String, String> {
        pairs(Url::parse(url).unwrap().query().unwrap())
    }

    fn message(e: OauthError) -> String {
        match e {
            OauthError::AuthRequired(m) | OauthError::Incompatible(m) | OauthError::Platform(m) => m,
        }
    }

    #[test]
    fn challenge_matches_the_rfc_7636_vector() {
        // RFC 7636 appendix B.
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn base64url_has_no_padding() {
        for (bytes, text) in [(&b""[..], ""), (b"f", "Zg"), (b"fo", "Zm8"), (b"foo", "Zm9v"), (&[0xfb, 0xff], "-_8")] {
            assert_eq!(base64url(bytes), text);
        }
    }

    #[test]
    fn verifier_is_43_url_safe_characters_and_never_repeats() {
        let (a, b) = (pkce().unwrap(), pkce().unwrap());
        for p in [&a, &b] {
            assert_eq!(p.verifier.expose().len(), 43);
            assert!(p.verifier.expose().bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
            assert_eq!(p.challenge, challenge(p.verifier.expose()));
            assert_eq!(p.state.len(), 22);
        }
        assert_ne!(a.verifier.expose(), b.verifier.expose());
        assert_ne!(a.state, b.state);
    }

    #[test]
    fn authorize_url_has_every_parameter() {
        let url = authorize_url("chal", "st ate");
        assert!(url.starts_with("https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize?"));
        // Well below the host's limit for a sign-in request's text.
        assert!(url.len() < 1000);
        let q = query_of(&url);
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["response_mode"], "query");
        assert_eq!(q["redirect_uri"], "https://login.microsoftonline.com/common/oauth2/nativeclient");
        assert_eq!(q["scope"], "https://api.spaces.skype.com/.default offline_access openid profile");
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "st ate");
        assert_eq!(q["prompt"], "select_account");
        assert_eq!(q["claims"], r#"{"access_token":{"xms_cc":{"values":["CP1"]}}}"#);
        assert_eq!(q.len(), 10);
    }

    #[test]
    fn redirect_gives_the_code_of_this_sign_in() {
        let code = redirect("https://login.microsoftonline.com/common/oauth2/nativeclient?code=c%2Fd-1&state=s1&session_state=x", "s1").unwrap();
        assert_eq!(code.expose(), "c/d-1");
    }

    #[test]
    fn redirect_for_another_state_is_refused() {
        for answer in [
            "https://x.example/?code=c1&state=other",
            "https://x.example/?code=c1",
            "https://x.example/?code=c1&state=",
            "https://x.example/?error=access_denied&state=other",
        ] {
            let e = redirect(answer, "s1").err().unwrap();
            assert_eq!(e, OauthError::AuthRequired("the sign-in answer is not for this sign-in".to_string()), "{answer}");
        }
        assert!(matches!(redirect("not a url", "s1"), Err(OauthError::AuthRequired(_))));
    }

    #[test]
    fn redirect_error_keeps_the_aadsts_number_only() {
        let description = "AADSTS65004: The user p@synthetic.example declined to consent.\r\nTrace ID: t-1";
        let answer =
            Url::parse_with_params("https://x.example/", [("error", "access_denied"), ("error_description", description), ("state", "s1")]).unwrap();
        let text = message(redirect(answer.as_str(), "s1").err().unwrap());
        assert_eq!(text, "Microsoft refused the sign-in (access_denied, AADSTS65004)");
        assert!(!text.contains("synthetic"));
        // Without a description, or with a code that is not one, only what is safe stays.
        let text = message(redirect("https://x.example/?error=bad%20code%0A&state=s1", "s1").err().unwrap());
        assert_eq!(text, "Microsoft refused the sign-in");
    }

    #[test]
    fn redirect_without_a_code_is_refused() {
        for answer in ["https://x.example/?state=s1", "https://x.example/?state=s1&code="] {
            let e = redirect(answer, "s1").err().unwrap();
            assert_eq!(e, OauthError::AuthRequired("the sign-in answer has no code".to_string()), "{answer}");
        }
    }

    #[test]
    fn code_form_carries_the_verifier_and_the_redirect() {
        let b = body(&code_form("c/1", "ver"));
        assert_eq!(b["client_id"], CLIENT_ID);
        assert_eq!(b["grant_type"], "authorization_code");
        assert_eq!(b["code"], "c/1");
        assert_eq!(b["code_verifier"], "ver");
        assert_eq!(b["redirect_uri"], REDIRECT_URI);
        assert_eq!(b["scope"], "https://api.spaces.skype.com/.default offline_access openid profile");
        assert_eq!(b["claims"], CAE_CLAIMS);
        assert_eq!(b.len(), 7);
        let request = token_request(Vec::new());
        assert_eq!(request.url, "https://login.microsoftonline.com/organizations/oauth2/v2.0/token");
        assert_eq!(request.method, "POST");
        assert!(request.headers.iter().any(|(n, v)| n == "Content-Type" && v == "application/x-www-form-urlencoded"));
        assert!(!request.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("origin")));
    }

    #[test]
    fn refresh_form_asks_for_the_scope() {
        let b = body(&refresh_form("rt+1", "https://ic3.teams.office.com/.default", "{\"a\":1}"));
        assert_eq!(b["client_id"], CLIENT_ID);
        assert_eq!(b["grant_type"], "refresh_token");
        assert_eq!(b["refresh_token"], "rt+1");
        assert_eq!(b["scope"], "https://ic3.teams.office.com/.default offline_access");
        assert_eq!(b["claims"], "{\"a\":1}");
        assert_eq!(b.len(), 5);
    }

    #[test]
    fn token_response_reads_the_known_fields() {
        let t = token_response(200, br#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3599,"scope":"x"}"#, true).unwrap();
        assert_eq!((t.access.expose(), t.refresh.as_ref().map(SecretString::expose), t.expires_in), ("at-1", Some("rt-1"), Some(3599)));
        // `expires_in` as a string; none, or nonsense, is none.
        let t = token_response(200, br#"{"access_token":"at-1","expires_in":"60"}"#, false).unwrap();
        assert_eq!((t.expires_in, t.refresh.is_none()), (Some(60), true));
        for odd in [r#""x""#, "-5", "null", "1.5"] {
            let t = token_response(200, format!(r#"{{"access_token":"a","expires_in":{odd}}}"#).as_bytes(), false).unwrap();
            assert_eq!(t.expires_in, None, "{odd}");
        }
    }

    #[test]
    fn sign_in_needs_a_refresh_token_and_a_refresh_does_not() {
        let body = br#"{"access_token":"at-1","expires_in":3599}"#;
        assert_eq!(message(token_response(200, body, true).err().unwrap()), "unexpected server response at refresh_token");
        assert!(matches!(token_response(200, body, true), Err(OauthError::Incompatible(_))));
        let t = token_response(200, body, false).unwrap();
        assert!(t.refresh.is_none());
        // An empty refresh token is none.
        assert!(token_response(200, br#"{"access_token":"a","refresh_token":""}"#, true).is_err());
    }

    #[test]
    fn token_response_without_an_access_token_or_json_is_incompatible() {
        for (body, path) in [
            (&br#"{"refresh_token":"rt"}"#[..], "access_token"),
            (br#"{"access_token":""}"#, "access_token"),
            (br#"{"access_token":5}"#, "access_token"),
            (b"<html>", "$"),
            (b"", "$"),
        ] {
            let e = token_response(200, body, false).err().unwrap();
            assert_eq!(e, OauthError::Incompatible(format!("unexpected server response at {path}")));
        }
    }

    #[test]
    fn microsoft_asks_to_sign_in_again() {
        let body = br#"{"error":"invalid_grant","error_description":"AADSTS70000: p@synthetic.example","error_codes":[70000]}"#;
        assert_eq!(
            token_response(400, body, false).err(),
            Some(OauthError::AuthRequired("Microsoft asks to sign in again (AADSTS70000)".to_string()))
        );
        for error in ["interaction_required", "consent_required", "login_required"] {
            let body = format!(r#"{{"error":"{error}","error_description":"AADSTS50076: x"}}"#);
            assert_eq!(
                token_response(401, body.as_bytes(), false).err(),
                Some(OauthError::AuthRequired("Microsoft asks to sign in again (AADSTS50076)".to_string()))
            );
        }
        let e = token_response(400, br#"{"error":"invalid_grant"}"#, false).err().unwrap();
        assert_eq!(e, OauthError::AuthRequired("Microsoft asks to sign in again".to_string()));
    }

    #[test]
    fn other_microsoft_errors_name_the_code_only() {
        let body = br#"{"error":"invalid_client","error_description":"AADSTS7000218: p@synthetic.example Trace ID: t","error_codes":[7000218]}"#;
        let text = message(token_response(400, body, false).err().unwrap());
        assert_eq!(text, "the sign-in service refused the request (invalid_client, AADSTS7000218)");
        assert!(!text.contains("synthetic"));
        let text = message(token_response(400, br#"{"error":"p@synthetic.example"}"#, false).err().unwrap());
        assert_eq!(text, "the sign-in service refused the request");
    }

    #[test]
    fn an_error_that_is_not_microsoft_json_is_the_status() {
        for (status, body) in [(400, &b"<html>p@synthetic.example</html>"[..]), (401, b"{}"), (400, br#"{"error":5}"#), (503, b"x"), (404, b"{}")] {
            let e = token_response(status, body, false).err().unwrap();
            assert_eq!(e, OauthError::Platform(format!("the sign-in service answered HTTP {status}")));
        }
    }

    #[test]
    fn no_claims_challenge_is_cae_alone() {
        assert_eq!(merge_claims(None), r#"{"access_token":{"xms_cc":{"values":["CP1"]}}}"#);
    }

    #[test]
    fn a_challenge_object_gets_xms_cc() {
        let merged: Value = serde_json::from_str(&merge_claims(Some(r#"{"access_token":{"nbf":{"essential":true,"value":"1700000000"}}}"#))).unwrap();
        assert_eq!(merged, serde_json::json!({"access_token": {"nbf": {"essential": true, "value": "1700000000"}, "xms_cc": {"values": ["CP1"]}}}));
        // A challenge without `access_token` gets the object; one with its own `xms_cc` keeps it.
        let merged: Value = serde_json::from_str(&merge_claims(Some(r#"{"id_token":{"auth_time":{"essential":true}}}"#))).unwrap();
        assert_eq!(merged["id_token"], serde_json::json!({"auth_time": {"essential": true}}));
        assert_eq!(merged["access_token"], serde_json::json!({"xms_cc": {"values": ["CP1"]}}));
        let own = r#"{"access_token":{"xms_cc":{"values":["CP2"]}}}"#;
        assert_eq!(serde_json::from_str::<Value>(&merge_claims(Some(own))).unwrap(), serde_json::from_str::<Value>(own).unwrap());
    }

    #[test]
    fn a_challenge_that_is_not_an_object_goes_on_unchanged() {
        for odd in ["not json", "[1]", "\"x\"", "", r#"{"access_token":"x"}"#] {
            assert_eq!(merge_claims(Some(odd)), odd);
        }
    }

    #[test]
    fn aadsts_is_the_digits_after_the_prefix() {
        assert_eq!(aadsts("x AADSTS50076: y").as_deref(), Some("AADSTS50076"));
        assert_eq!(aadsts("AADSTS12345678901234").as_deref(), Some("AADSTS1234567890"));
        for none in ["", "AADSTS", "AADSTS: x", "aadsts1"] {
            assert_eq!(aadsts(none), None, "{none}");
        }
    }
}
