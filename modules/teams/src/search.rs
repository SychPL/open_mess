//! Message search through Substrate (docs/protocols/teams.md 6): the request and the result
//! decoder. The entity type, content source, scenario, anchor format and result fields are
//! UNCONFIRMED until a live capture (F2b); an answer in another shape is incompatible, nothing is
//! guessed.

use serde_json::{Value, json};

use crate::chat::{obj, opt_str, req_str};
use crate::http::HttpRequest;
use crate::{ProtoError, incompatible, parse_json, time};

const QUERY_URL: &str = "https://substrate.office.com/search/api/v2/query";

/// One message found. No `Debug`: it carries message text.
pub struct Hit {
    /// `Source.ItemId`.
    pub id: String,
    /// `Source.ClientConversationId`, the chat service's conversation id.
    pub conversation: String,
    /// `Source.DateTimeReceived`.
    pub sent_ms: i64,
    /// `Source.Preview`, plain text.
    pub text: String,
    /// `Source.From.EmailAddress.Name` and `.Address`.
    pub sender_name: Option<String>,
    pub sender_address: Option<String>,
}

/// `anchor` is the `X-AnchorMailbox` value, `cvid` a fresh UUID; the caller adds the bearer token
/// for `https://outlook.office.com`.
pub fn request(query: &str, size: u32, anchor: &str, cvid: &str) -> HttpRequest {
    let body = json!({
        "entityRequests": [{
            "entityType": "Message",
            "contentSources": ["Teams"],
            "query": {"queryString": query, "displayQueryString": query},
            "from": 0,
            "size": size,
        }],
        "cvid": cvid,
        "scenario": {"name": "powerbar"},
    });
    let headers = [("Content-Type", "application/json"), ("X-ClientType", "MicrosoftTeams"), ("X-AnchorMailbox", anchor)]
        .map(|(name, value)| (name.to_string(), value.to_string()));
    HttpRequest { method: "POST".to_string(), url: QUERY_URL.to_string(), headers: headers.to_vec(), body: body.to_string().into_bytes() }
}

/// `EntitySets[].ResultSets[].Results[].Source`.
pub fn decode(body: &[u8]) -> Result<Vec<Hit>, ProtoError> {
    let v = parse_json(body)?;
    let sets = v.get("EntitySets").and_then(Value::as_array).ok_or_else(|| incompatible("EntitySets"))?;
    let mut hits = Vec::new();
    for (i, set) in sets.iter().enumerate() {
        for (j, results) in list(set, &format!("EntitySets[{i}]"), "ResultSets")?.iter().enumerate() {
            let path = format!("EntitySets[{i}].ResultSets[{j}]");
            for (k, result) in list(results, &path, "Results")?.iter().enumerate() {
                hits.push(hit(result, &format!("{path}.Results[{k}]"))?);
            }
        }
    }
    Ok(hits)
}

/// `v.key` as a list; absent or null is empty.
fn list<'a>(v: &'a Value, path: &str, key: &str) -> Result<&'a [Value], ProtoError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(incompatible(&format!("{path}.{key}"))),
    }
}

fn hit(result: &Value, path: &str) -> Result<Hit, ProtoError> {
    let source = result.get("Source").filter(|s| s.is_object()).ok_or_else(|| incompatible(&format!("{path}.Source")))?;
    let path = format!("{path}.Source");
    let received = req_str(source, &path, "DateTimeReceived")?;
    let address = obj(obj(source, &path, "From")?, &path, "EmailAddress")?;
    Ok(Hit {
        id: req_str(source, &path, "ItemId")?.to_string(),
        conversation: req_str(source, &path, "ClientConversationId")?.to_string(),
        sent_ms: time::iso8601_ms(received).ok_or_else(|| incompatible(&format!("{path}.DateTimeReceived")))?,
        text: opt_str(source, &path, "Preview")?.unwrap_or_default().to_string(),
        sender_name: opt_str(address, &path, "Name")?.map(str::to_string),
        sender_address: opt_str(address, &path, "Address")?.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_queries_teams_messages() {
        let r = request("budżet", 30, "Oid:o-1@t-1", "c-1");
        assert_eq!((r.method.as_str(), r.url.as_str()), ("POST", "https://substrate.office.com/search/api/v2/query"));
        assert!(r.headers.contains(&("X-AnchorMailbox".to_string(), "Oid:o-1@t-1".to_string())));
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["entityRequests"][0]["query"]["queryString"], "budżet");
        assert_eq!((body["entityRequests"][0]["size"].as_u64(), body["cvid"].as_str()), (Some(30), Some("c-1")));
    }

    #[test]
    fn decodes_hits_unconfirmed_shape() {
        let body = json!({"EntitySets": [{"ResultSets": [{"Results": [
            {"Source": {"ItemId": "i-1", "ClientConversationId": "19:a@thread.v2", "DateTimeReceived": "2027-01-15T08:00:00Z",
                        "Preview": "synthetic text", "From": {"EmailAddress": {"Name": "Synthetic Person", "Address": "p@synthetic.example"}}}},
            {"Source": {"ItemId": "i-2", "ClientConversationId": "19:b@thread.v2", "DateTimeReceived": "2027-01-15T08:00:01.5Z"}},
        ]}]}]});
        let hits = decode(body.to_string().as_bytes()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].id.as_str(), hits[0].conversation.as_str(), hits[0].sent_ms), ("i-1", "19:a@thread.v2", 1_800_000_000_000));
        assert_eq!((hits[0].text.as_str(), hits[0].sender_name.as_deref()), ("synthetic text", Some("Synthetic Person")));
        assert_eq!((hits[1].text.as_str(), hits[1].sender_address.as_deref()), ("", None));
        // No hits at all is an empty list.
        assert!(decode(br#"{"EntitySets": [{"ResultSets": [{}]}]}"#).unwrap().is_empty());
    }

    #[test]
    fn other_shapes_are_incompatible() {
        for (body, path) in [
            (r#"{}"#, "EntitySets"),
            (r#"{"EntitySets": [{"ResultSets": [{"Results": [{"Source": {"ItemId": "i"}}]}]}]}"#, "DateTimeReceived"),
            (r#"{"EntitySets": [{"ResultSets": [{"Results": [{}]}]}]}"#, "Source"),
            (r#"{"EntitySets": [{"ResultSets": 3}]}"#, "ResultSets"),
        ] {
            match decode(body.as_bytes()) {
                Err(ProtoError::Incompatible(p)) => assert!(p.ends_with(path), "{p}"),
                Ok(_) => panic!("{body}"),
                Err(e) => panic!("{e:?}"),
            }
        }
    }
}
